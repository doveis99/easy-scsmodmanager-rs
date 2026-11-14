import { Channel, invoke } from "@tauri-apps/api/core";
import { open as pickOpen, save as pickSave } from "@tauri-apps/plugin-dialog";
import { $, confirmModal, dismissDialog, h, infoModal, openDialog } from "./dom";
import { LANGS, applyStatic, hasKey, locale, num, resolveLang, setLang, t } from "./i18n";

// ---------------------------------------------------------------- types

type GameId = "ets2" | "ats";
type Compat = "compatible" | "incompatible" | "unspecified" | "unknown_game_version";

interface InstallInfo {
  game: GameId;
  short_name: string;
  documents_dir: string;
  workshop_dir: string | null;
  version: string | null;
  manual: boolean;
}

/** One thing the content analysis saw: files below a path pattern, or
 *  files read that define a unit class. */
interface Signal {
  label: string;
  key: string;
  count: number;
  from_content: boolean;
}

interface Mod {
  id: string;
  path: string;
  file_name: string;
  source: "local" | "workshop";
  workshop_url: string | null;
  format: string;
  display_name: string;
  author: string;
  version: string;
  categories: string[];
  manifest_categories: string[];
  /** Categories pinned by hand, the main one first; empty when automatic. */
  category_pins: string[];
  /** What the categories would be without pins. */
  auto_categories: string[];
  /** The automatic categories come from the files, not from the manifest. */
  judged_from_content: boolean;
  /** A category the files point to, offered but never applied by itself. */
  suggested_category: string | null;
  /** What the analysis saw in the files, the strongest signals first. */
  evidence: Signal[];
  /** Files a locked archive holds without listing them, and how many of
   *  those could be named all the same. */
  hidden_files: number;
  hidden_named: number;
  group_token: string;
  is_map: boolean;
  error: string | null;
  installed_at: number;
  description: string | null;
  compatible_versions: string[];
  compat: Compat;
  has_icon: boolean;
  favorite: boolean;
}

interface Profile {
  path: string;
  profile_name: string;
  active_count: number;
  error: string | null;
}

interface ActiveItem {
  name: string;
  display_name: string;
  token: string;
  installed: boolean;
  compat: Compat;
}

type Row =
  | { kind: "spacer"; group_id: string }
  | { kind: "mod"; index: number; misplaced: boolean; expected_group_id: string };

interface Layout {
  rows: Row[];
  misplaced: number;
}

interface ModOverride {
  severity: "partial" | "full";
  lost: { file: string; winner: string }[];
  shared_total: number;
}

interface ConflictReport {
  /** Mods that lose def files to a mod above them, keyed by active name. */
  overrides: Record<string, ModOverride>;
  /** Files shared by so many mods that the overlap is normal, not a conflict. */
  frequent: Record<string, { file: string; owners: number }[]>;
}

interface GameSettings {
  documents_dir: string | null;
  workshop_dir: string | null;
  last_profile: string | null;
}

/** Sort order and filters of the library, kept across restarts. */
interface ViewSettings {
  sort: string;
  category: string;
  source: string;
  favorites: boolean;
}

interface Settings {
  workshop_lookup: boolean;
  backup_keep: number;
  map_base_names: string[];
  last_game: GameId | null;
  /** Interface language (`ko`, `en`, `zh`); null follows the system. */
  language: string | null;
  view: ViewSettings;
  ets2: GameSettings;
  ats: GameSettings;
}

interface SettingsView {
  settings: Settings;
  detected: Partial<Record<GameId, { documents_dir: string | null; workshop_dir: string | null }>>;
  backup_root: string;
}

interface BackupDto {
  path: string;
  timestamp: string;
  size_bytes: number;
}

interface SaveResult {
  backup: string;
  pruned: number;
}

interface DeleteResult {
  deleted: string[];
  failed: { id: string; error: string }[];
  skipped_workshop: number;
}

interface ExtractOutcome {
  /** Folder the files went to; null when nothing could be written. */
  dest: string | null;
  total: number;
  extracted: number;
  failed: number;
  cancelled: boolean;
  /** Files a locked archive does not list whose path was recovered. */
  recovered: number;
  /** Files whose path stayed unknown; written by hash into `_unnamed`. */
  unnamed: number;
  failures: { entry: string; error: string }[];
}

interface WorkshopFetch {
  asked: number;
  titles: number;
  previews: number;
  error: string | null;
}

interface SortPlan {
  /** Permutation that sorts into groups without changing any file's winner. */
  order: number[];
  sorted: boolean;
  /** Mods kept outside their group by a mod they share files with. */
  held: string[];
  partners: Record<string, string[]>;
}

const NO_PLAN: SortPlan = { order: [], sorted: true, held: [], partners: {} };

interface Constants {
  categories: string[];
  groups: string[];
  /** category token -> id of the load-order group it sorts into. */
  category_groups: Record<string, string>;
}

// --------------------------------------------------------------- labels

/** A token the game may add later simply shows as it is. */
const catLabel = (token: string) => {
  const key = `cat.${token}`;
  return hasKey(key) ? t(key) : token;
};
const groupLabel = (id: string) => {
  const key = `group.${id}`;
  return hasKey(key) ? t(key) : id;
};

// ---------------------------------------------------------------- state

const state = {
  constants: { categories: [], groups: [], category_groups: {} } as Constants,
  installs: [] as InstallInfo[],
  game: "ets2" as GameId,
  mods: [] as Mod[],
  profiles: [] as Profile[],
  profilePath: "",
  active: [] as ActiveItem[],
  /** JSON of the list as last loaded/saved - the dirty check. */
  saved: "[]",
  layout: { rows: [], misplaced: 0 } as Layout,
  conflicts: { overrides: {}, frequent: {} } as ConflictReport,
  plan: NO_PLAN,
  filter: { q: "", category: "", source: "", sort: "name", favorites: false },
  settings: null as SettingsView | null,
  /** Bumped per load so a slow response for an old game/profile is dropped. */
  epoch: 0,
};

const icons = new Map<string, string | null>();

function setStatus(message: string, kind: "info" | "error" | "ok" = "info") {
  const el = $("status");
  el.textContent = message;
  el.dataset.kind = kind;
}

/** Errors of the backend arrive as "@code<TAB>value..." so that they can
 *  be shown in the interface language; anything else is shown as it is. */
function backendText(raw: string): string {
  if (!raw.startsWith("@")) return raw;
  const [code, ...values] = raw.slice(1).split("\t");
  const key = `err.${code}`;
  return hasKey(key) ? t(key, Object.fromEntries(values.map((v, i) => [String(i), v]))) : raw;
}

function errorText(e: unknown): string {
  return backendText(e instanceof Error ? e.message : String(e));
}

// -------------------------------------------------------------- loading

const isDirty = () => JSON.stringify(state.active) !== state.saved;

async function loadGame(game: GameId) {
  const epoch = ++state.epoch;
  state.game = game;
  state.mods = [];
  state.profiles = [];
  state.active = [];
  state.saved = "[]";
  state.profilePath = "";
  // the old layout indexes into the list that was just cleared
  state.layout = { rows: [], misplaced: 0 };
  state.conflicts = { overrides: {}, frequent: {} };
  state.plan = NO_PLAN;
  renderGameSwitch();
  renderProfileSelect();
  renderAll();
  setStatus(t("status.scanning"));
  const started = performance.now();
  try {
    const [mods, profiles] = await Promise.all([
      invoke<Mod[]>("scan_mods", { game }),
      invoke<Profile[]>("list_profiles", { game }),
    ]);
    if (epoch !== state.epoch) return;
    state.mods = mods;
    state.profiles = profiles;
    // default to the profile with the most active mods
    const usable = profiles.filter((p) => !p.error);
    const best = usable.reduce<Profile | null>(
      (a, b) => (a && a.active_count >= b.active_count ? a : b),
      null,
    );
    // reopen the profile used last time if it is still there
    const last = state.settings?.settings[game].last_profile;
    state.profilePath = usable.find((p) => p.path === last)?.path ?? best?.path ?? "";
    renderProfileSelect();
    await loadProfile();
    rememberPlace();
    const unreadable = mods.filter((m) => m.error).length;
    const ms = Math.round(performance.now() - started);
    setStatus(
      t("status.scanned", { n: mods.length, ms }) +
        (unreadable ? t("status.unreadable", { n: unreadable }) : ""),
      "ok",
    );
    void refreshWorkshop();
  } catch (e) {
    if (epoch !== state.epoch) return;
    renderAll();
    setStatus(errorText(e), "error");
  }
}

/** Ask Steam for names and previews of workshop mods that lack them. Runs
 *  in the background after a scan; the grid is redrawn only if something
 *  new arrived. Being offline is not an error worth interrupting for. */
async function refreshWorkshop() {
  const epoch = state.epoch;
  const game = state.game;
  try {
    const r = await invoke<WorkshopFetch>("fetch_workshop_meta", { game });
    if (epoch !== state.epoch) return;
    if (r.titles + r.previews === 0) {
      if (r.error) console.warn("workshop lookup:", r.error);
      return;
    }
    const mods = await invoke<Mod[]>("mods_snapshot", { game });
    if (epoch !== state.epoch) return;
    state.mods = mods;
    // cards that had no icon before may have a preview now
    for (const [key, url] of icons) if (url === null) icons.delete(key);
    renderLibrary();
    setStatus(t("status.workshopFetched", { titles: r.titles, previews: r.previews }), "ok");
  } catch (e) {
    console.warn("workshop lookup failed:", errorText(e));
  }
}

/** Note the current game and profile for the next start (best effort). */
function rememberPlace() {
  const game = state.game;
  const profilePath = state.profilePath || null;
  const gs = state.settings?.settings[game];
  if (gs && profilePath) gs.last_profile = profilePath;
  invoke("remember_place", { game, profilePath }).catch(() => {});
}

async function loadProfile() {
  const epoch = state.epoch;
  if (!state.profilePath) {
    state.active = [];
  } else {
    const items = await invoke<ActiveItem[]>("read_load_order", {
      game: state.game,
      profilePath: state.profilePath,
    });
    if (epoch !== state.epoch) return;
    state.active = items;
  }
  state.saved = JSON.stringify(state.active);
  await relayout();
}

/** The list as the backend wants it: identity only, display order. */
function activeEntries(items: ActiveItem[] = state.active) {
  return items.map(({ name, display_name }) => ({ name, display_name }));
}

/** Recompute group headers and conflicts for the current list and redraw. */
async function relayout() {
  const epoch = state.epoch;
  const [layout, conflicts, plan] = await Promise.all([
    invoke<Layout>("layout_rows", { tokens: state.active.map((a) => a.token) }),
    invoke<ConflictReport>("analyze_conflicts", { game: state.game, items: activeEntries() }),
    invoke<SortPlan>("sort_plan", { game: state.game, items: activeEntries() }),
  ]);
  if (epoch !== state.epoch) return;
  state.layout = layout;
  state.conflicts = conflicts;
  state.plan = plan;
  renderAll();
}

// ------------------------------------------------------------ conflicts

const GLYPH = { full: "⊘", partial: "⚠", frequent: "=" } as const;

function displayNameOf(name: string): string {
  return state.active.find((a) => a.name === name)?.display_name || name;
}

function gameVersion(): string | null {
  return state.installs.find((i) => i.game === state.game)?.version ?? null;
}

/** What one mod loses and shares, as text. `limit` caps each list (tooltips). */
function conflictLines(name: string, limit?: number): string[] {
  const lines: string[] = [];
  const more = (rest: number) => {
    if (rest > 0) lines.push(t("common.more", { n: rest }));
  };
  const override = state.conflicts.overrides[name];
  if (override) {
    lines.push(
      override.severity === "full"
        ? t("conflict.allLost", { total: override.shared_total })
        : t("conflict.someLost", { total: override.shared_total, lost: override.lost.length }),
    );
    const shown = limit === undefined ? override.lost : override.lost.slice(0, limit);
    for (const l of shown) lines.push(`  ${l.file} → ${displayNameOf(l.winner)}`);
    more(override.lost.length - shown.length);
  }
  const frequent = state.conflicts.frequent[name];
  if (frequent) {
    lines.push(
      override
        ? t("conflict.frequentToo")
        : t("conflict.frequentOnly"),
    );
    const shown = limit === undefined ? frequent : frequent.slice(0, limit);
    for (const f of shown) lines.push(t("conflict.owners", { file: f.file, n: f.owners }));
    more(frequent.length - shown.length);
  }
  return lines;
}

/** The whole report, one block per affected mod, in load order. */
function conflictReport(): string {
  const affected = state.active.filter(
    (a) => state.conflicts.overrides[a.name] || state.conflicts.frequent[a.name],
  );
  const profile = state.profiles.find((p) => p.path === state.profilePath)?.profile_name ?? "";
  const version = gameVersion();
  const head = [
    t("conflict.head", { game: state.game.toUpperCase() + (version ? ` ${version}` : ""), profile }),
    t("conflict.count", { n: Object.keys(state.conflicts.overrides).length }),
    t("conflict.note"),
    "=".repeat(44),
  ];
  const blocks = affected.map((a) => [a.display_name || a.name, ...conflictLines(a.name)].join("\n"));
  return [head.join("\n"), ...blocks].join("\n\n");
}

function conflictGlyph(item: ActiveItem): HTMLElement | null {
  const override = state.conflicts.overrides[item.name];
  const frequent = state.conflicts.frequent[item.name];
  if (!override && !frequent) return null;
  const kind = override ? override.severity : "frequent";
  const label = t(`conflict.${kind}`);
  return h(
    "button",
    {
      class: `glyph ${kind}`,
      title: [label, ...conflictLines(item.name, 8)].join("\n"),
      "aria-label": label,
      onclick: () => void infoModal(item.display_name || item.name, conflictLines(item.name).join("\n")),
    },
    GLYPH[kind],
  );
}

function compatTitle(versions: string[]): string {
  const game = gameVersion();
  return t("compat.declared", { versions: versions.join(", ") }) + (game ? t("compat.current", { version: game }) : "");
}

// ------------------------------------------------------------ rendering

function renderAll() {
  renderLibrary();
  renderOrder();
}

function renderGameSwitch() {
  const box = $("game-switch");
  box.replaceChildren(
    ...(["ets2", "ats"] as GameId[]).map((g) => {
      const install = state.installs.find((i) => i.game === g);
      return h(
        "button",
        {
          class: "seg-btn" + (state.game === g ? " on" : ""),
          role: "tab",
          "aria-selected": String(state.game === g),
          disabled: !install,
          title: install ? install.documents_dir : t("game.notFound"),
          onclick: () => void switchGame(g),
        },
        g.toUpperCase(),
      );
    }),
  );
  const current = state.installs.find((i) => i.game === state.game);
  $("game-version").textContent = current?.version ? t("game.version", { version: current.version }) : "";
}

async function switchGame(game: GameId) {
  if (game === state.game) return;
  if (isDirty() && !(await confirmDiscard())) return;
  await loadGame(game);
}

function confirmDiscard(): Promise<boolean> {
  return confirmModal(
    t("discard.title"),
    t("discard.body"),
    t("discard.ok"),
  );
}

function renderProfileSelect() {
  const select = $<HTMLSelectElement>("profile-select");
  select.replaceChildren(
    ...state.profiles.map((p) =>
      h(
        "option",
        { value: p.path, disabled: !!p.error, selected: p.path === state.profilePath },
        p.error ? t("profile.unreadable", { name: p.profile_name }) : t("profile.active", { name: p.profile_name, n: p.active_count }),
      ),
    ),
  );
  if (state.profiles.length === 0) {
    select.append(h("option", { value: "" }, t("profile.none")));
  }
  select.disabled = state.profiles.length === 0;
}

function filteredMods(): Mod[] {
  const { q, category, source, sort, favorites } = state.filter;
  const needle = q.trim().toLowerCase();
  const out = state.mods.filter((m) => {
    if (category && !m.categories.includes(category)) return false;
    if (source && m.source !== source) return false;
    if (favorites && !m.favorite) return false;
    if (!needle) return true;
    const hay = [m.display_name, m.author, m.file_name, ...m.categories.map(catLabel), ...m.categories]
      .join("\n")
      .toLowerCase();
    return hay.includes(needle);
  });
  if (sort === "date") {
    out.sort((a, b) => b.installed_at - a.installed_at);
  } else {
    out.sort((a, b) => a.display_name.localeCompare(b.display_name, locale()));
  }
  return out;
}

const iconObserver = new IntersectionObserver(
  (entries) => {
    for (const entry of entries) {
      if (!entry.isIntersecting) continue;
      iconObserver.unobserve(entry.target);
      void loadIcon(entry.target as HTMLElement);
    }
  },
  { rootMargin: "300px" },
);

async function loadIcon(holder: HTMLElement) {
  const id = holder.dataset.modId;
  const game = state.game;
  if (!id) return;
  const key = `${game}/${id}`;
  if (!icons.has(key)) {
    try {
      icons.set(key, await invoke<string | null>("mod_icon", { game, id }));
    } catch {
      icons.set(key, null);
    }
  }
  const url = icons.get(key);
  if (url && holder.isConnected) {
    holder.replaceChildren(h("img", { src: url, alt: "", loading: "lazy" }));
  }
}

function activeNames(): Set<string> {
  return new Set(state.active.map((a) => a.name));
}

function renderLibrary() {
  const mods = filteredMods();
  const active = activeNames();
  $("library-count").textContent =
    mods.length === state.mods.length
      ? t("lib.count", { n: state.mods.length })
      : t("lib.countShown", { total: state.mods.length, shown: mods.length });

  const grid = $("grid");
  if (mods.length === 0) {
    grid.replaceChildren(
      h("div", { class: "empty" }, state.mods.length ? t("lib.noMatch") : t("lib.empty")),
    );
    return;
  }
  grid.replaceChildren(...mods.map((m) => modCard(m, active.has(m.id))));
}

/** Values of the card's category select that are not category tokens. */
const MULTI_PINS = "__multi";
const EDIT_PINS = "__edit";

/** The strongest things the content analysis saw, as one line. */
function evidenceText(m: Mod): string {
  return m.evidence
    .slice(0, 5)
    .map((s) => `${s.from_content ? t("evidence.unit", { key: s.key }) : s.key} ×${num(s.count)}`)
    .join(", ");
}

function lockedTitle(m: Mod): string {
  return (
    t("locked.title", { hidden: num(m.hidden_files), named: num(m.hidden_named) })
  );
}

function modCard(m: Mod, isActive: boolean): HTMLElement {
  const pins = m.category_pins;
  const current = pins.length === 0 ? "" : pins.length === 1 ? pins[0] : MULTI_PINS;
  const iconHolder = h("div", { class: "card-icon", "data-mod-id": m.id }, h("span", {}, "SCS"));
  if (m.has_icon) {
    const cached = icons.get(`${state.game}/${m.id}`);
    if (cached) iconHolder.replaceChildren(h("img", { src: cached, alt: "" }));
    else iconObserver.observe(iconHolder);
  }

  const categorySelect = h(
    "select",
    {
      class: "cat-select",
      // the control is narrow: the full pinned list goes into the tooltip
      title: pins.length ? t("pin.manualTitle", { list: pins.map(catLabel).join(" + "), main: catLabel(pins[0]) }) : t("pin.change"),
      "aria-label": t("pin.change"),
      onchange: (ev: Event) => {
        const select = ev.target as HTMLSelectElement;
        if (select.value === EDIT_PINS) {
          // not a category: put the choice back and open the editor
          select.value = current;
          void openCategoryDialog(m);
        } else if (select.value !== MULTI_PINS) {
          void setOverride(m, select.value ? [select.value] : []);
        }
      },
    },
    h("option", { value: "", selected: current === "" }, t("pin.auto")),
    pins.length > 1
      ? h("option", { value: MULTI_PINS, selected: true }, t("pin.manual", { list: pins.map(catLabel).join(" + ") }))
      : null,
    ...state.constants.categories.map((c) => h("option", { value: c, selected: current === c }, catLabel(c))),
    h("option", { value: EDIT_PINS }, t("pin.editMany")),
  );
  const locked = m.hidden_files > 0;

  const meta = [m.author, m.version && `v${m.version}`].filter(Boolean).join(" · ");
  return h(
    "article",
    { class: "card" + (isActive ? " active" : "") + (m.error ? " broken" : ""), title: m.description ?? m.path },
    iconHolder,
    h(
      "div",
      { class: "card-body" },
      h("button", { class: "card-title", title: t("card.details"), onclick: () => void openDetails(m) }, m.display_name),
      h("div", { class: "card-meta muted" }, meta || m.file_name),
      h(
        "div",
        { class: "badges" },
        ...m.categories.map((c) =>
          h("span", { class: "badge cat" + (pins.length ? " pinned" : "") }, catLabel(c)),
        ),
        h("span", { class: "badge src" }, m.source === "workshop" ? t("badge.workshop") : t("badge.local")),
        locked ? h("span", { class: "badge keep", title: lockedTitle(m) }, t("badge.locked")) : null,
        m.suggested_category
          ? h(
              "span",
              {
                class: "badge suggest",
                title: t("suggest.title", { cat: catLabel(m.suggested_category), why: evidenceText(m) }),
              },
              t("badge.suggest", { cat: catLabel(m.suggested_category) }),
            )
          : null,
        m.compat === "incompatible"
          ? h("span", { class: "badge err", title: compatTitle(m.compatible_versions) }, t("badge.incompatible"))
          : null,
        m.error ? h("span", { class: "badge err", title: m.error }, t("badge.unreadable")) : null,
      ),
    ),
    h(
      "div",
      { class: "card-actions" },
      h(
        "div",
        { class: "card-actions-left" },
        h(
          "button",
          {
            class: "star" + (m.favorite ? " on" : ""),
            title: m.favorite ? t("fav.remove") : t("fav.add"),
            "aria-label": m.favorite ? t("fav.remove") : t("fav.add"),
            "aria-pressed": String(m.favorite),
            onclick: () => void toggleFavorite(m),
          },
          m.favorite ? "★" : "☆",
        ),
        categorySelect,
      ),
      h(
        "button",
        {
          class: "btn small" + (isActive ? "" : " primary"),
          onclick: () => void (isActive ? removeActive(m.id) : addActive(m)),
        },
        isActive ? t("card.deactivate") : t("card.activate"),
      ),
    ),
  );
}

function renderOrder() {
  const rowsEl = $("rows");
  const { rows } = state.layout;
  // a mod outside its group is only a problem when nothing holds it there
  const held = new Set(state.plan.held);
  const outside = rows.flatMap((r) => (r.kind === "mod" && r.misplaced ? [state.active[r.index]?.name] : []));
  const heldCount = outside.filter((n) => n !== undefined && held.has(n)).length;
  const misplaced = outside.length - heldCount;
  const dirty = isDirty();

  const overridden = Object.keys(state.conflicts.overrides).length;
  const incompatible = state.active.filter((a) => a.compat === "incompatible").length;
  $("order-summary").textContent =
    t("order.active", { n: state.active.length }) +
    (misplaced ? t("order.misplaced", { n: misplaced }) : "") +
    (heldCount ? t("order.held", { n: heldCount }) : "") +
    (overridden ? t("order.overridden", { n: overridden }) : "") +
    (incompatible ? t("order.incompatible", { n: incompatible }) : "");
  $("show-conflicts").hidden =
    overridden === 0 && Object.keys(state.conflicts.frequent).length === 0;
  $<HTMLButtonElement>("auto-sort").disabled = state.plan.sorted;
  $<HTMLButtonElement>("revert").disabled = !dirty;
  $<HTMLButtonElement>("save").disabled = !dirty || !state.profilePath;

  // where a drop on each row lands: a header inserts before the next mod
  let nextIndex = state.active.length;
  const insertAt: number[] = new Array(rows.length);
  for (let i = rows.length - 1; i >= 0; i--) {
    const row = rows[i];
    if (row.kind === "mod") nextIndex = row.index;
    insertAt[i] = nextIndex;
  }
  const counts = new Map<string, number>();
  let currentGroup = "";
  for (const row of rows) {
    if (row.kind === "spacer") currentGroup = row.group_id;
    else counts.set(currentGroup, (counts.get(currentGroup) ?? 0) + 1);
  }

  const tail = h("div", { class: "row tail", "aria-hidden": "true" });
  tail.dataset.insertAt = String(state.active.length);
  rowsEl.replaceChildren(
    ...rows.flatMap((row, i) => {
      // a layout that is momentarily out of step with the list draws nothing
      if (row.kind === "mod" && !state.active[row.index]) return [];
      const el =
        row.kind === "spacer"
          ? h(
              "div",
              { class: "row spacer" },
              h("span", {}, groupLabel(row.group_id)),
              h("span", { class: "muted" }, String(counts.get(row.group_id) ?? 0)),
            )
          : orderRow(row, state.active[row.index]);
      el.dataset.insertAt = String(insertAt[i]);
      return [el];
    }),
    tail,
  );
}

function orderRow(row: Extract<Row, { kind: "mod" }>, item: ActiveItem): HTMLElement {
  return h(
    "div",
    {
      class:
        "row mod" +
        (row.misplaced && !state.plan.held.includes(item.name) ? " misplaced" : "") +
        (item.installed ? "" : " missing"),
      tabindex: "0",
      "data-index": String(row.index),
    },
    h("span", { class: "grip", "aria-hidden": "true" }, "⋮⋮"),
    h("span", { class: "row-name" }, item.display_name || item.name),
    row.misplaced ? placementBadge(row, item) : null,
    item.installed ? null : h("span", { class: "badge err" }, t("badge.notInstalled")),
    item.compat === "incompatible"
      ? h(
          "span",
          {
            class: "badge err",
            title: compatTitle(state.mods.find((m) => m.id === item.name)?.compatible_versions ?? []),
          },
          t("badge.incompatible"),
        )
      : null,
    conflictGlyph(item),
    h(
      "button",
      { class: "icon-btn", title: t("card.deactivate"), "aria-label": t("card.deactivate"), onclick: () => void removeActive(item.name) },
      "✕",
    ),
  );
}

// -------------------------------------------------------------- actions

async function addActive(m: Mod) {
  if (activeNames().has(m.id)) return;
  // new mods go on top; the layout flags them if that is the wrong group
  state.active.unshift({
    name: m.id,
    display_name: m.display_name,
    token: m.group_token,
    installed: true,
    compat: m.compat,
  });
  await relayout();
}

async function removeActive(name: string) {
  state.active = state.active.filter((a) => a.name !== name);
  await relayout();
}

async function moveActive(from: number, insertAt: number) {
  if (from < 0 || from >= state.active.length) return;
  const target = insertAt > from ? insertAt - 1 : insertAt;
  if (target === from) return;
  const [item] = state.active.splice(from, 1);
  state.active.splice(target, 0, item);
  await relayout();
}

/** Badge for a mod that sits outside its group: a warning when it could
 *  simply move there, a neutral note when a mod it shares files with holds
 *  it where it is (a patch above what it patches). */
function placementBadge(row: Extract<Row, { kind: "mod" }>, item: ActiveItem): HTMLElement {
  const group = groupLabel(row.expected_group_id);
  if (!state.plan.held.includes(item.name)) {
    return h("span", { class: "badge warn", title: t("placement.belongs", { group }) }, `→ ${group}`);
  }
  const partners = (state.plan.partners[item.name] ?? []).map(displayNameOf);
  const shown = partners.slice(0, 6).join(", ") + (partners.length > 6 ? t("placement.andMore", { n: partners.length - 6 }) : "");
  return h(
    "span",
    {
      class: "badge keep",
      title:
        t("placement.heldTitle", { group, mods: shown }),
    },
    t("badge.held"),
  );
}

/** Sort mods into their groups. Mods that share files keep their relative
 *  order, so the sort never changes which mod wins a file. */
async function autoSort() {
  const { order, held } = state.plan;
  if (order.length !== state.active.length) return; // plan is for another list
  state.active = order.map((i) => state.active[i]);
  await relayout();
  setStatus(
    t("sort.done") +
      (held.length ? t("sort.held", { n: held.length }) : "") +
      t("sort.saveNote"),
  );
}

async function revert() {
  state.active = JSON.parse(state.saved) as ActiveItem[];
  await relayout();
  setStatus(t("status.reverted"));
}

async function save() {
  if (!state.profilePath || !isDirty()) return;
  const game = state.game.toUpperCase();
  const ok = await confirmModal(
    t("save.title"),
    t("save.body", { game }),
    t("common.save"),
  );
  if (!ok) return;
  try {
    const result = await invoke<SaveResult>("save_load_order", {
      game: state.game,
      profilePath: state.profilePath,
      items: activeEntries(),
    });
    state.saved = JSON.stringify(state.active);
    const profile = state.profiles.find((p) => p.path === state.profilePath);
    if (profile) profile.active_count = state.active.length;
    renderProfileSelect();
    renderOrder();
    setStatus(
      t("save.done", { backup: result.backup }) + (result.pruned ? t("save.pruned", { n: result.pruned }) : ""),
      "ok",
    );
  } catch (e) {
    setStatus(t("save.failed", { error: errorText(e) }), "error");
  }
}

/** Pick every category a mod covers, and which of them is the main
 *  one. For mods that are several things at once, and for locked archives
 *  whose content cannot tell what they are. */
async function openCategoryDialog(m: Mod) {
  const categories = state.constants.categories;
  // start from the pins, or from what the automatic result says
  const chosen = new Set(m.category_pins.length ? m.category_pins : m.auto_categories);
  let main = (m.category_pins.length ? m.category_pins : m.auto_categories)[0] ?? "";
  // A main category the user picked stays; otherwise it follows the
  // selection. A pin that is being edited was picked by the user.
  let mainPicked = m.category_pins.length > 0;
  let selectionChanged = false;
  /** Of several categories, the one whose load-order group sits highest.
   *  A mod that covers several things is mostly there to override them,
   *  and only wins all of its files from the highest of its groups. Where
   *  it is the other way round (a truck that brings its own sounds), the
   *  user marks the main category by hand. */
  const highest = (cats: Iterable<string>) => {
    const { groups, category_groups } = state.constants;
    const rank = (c: string) => {
      const group = groups.indexOf(category_groups[c] ?? "");
      return group < 0 ? groups.length : group;
    };
    const byRank = (a: string, b: string) => rank(a) - rank(b) || categories.indexOf(a) - categories.indexOf(b);
    return [...cats].sort(byRank)[0] ?? "";
  };
  const error = h("div", { class: "form-error", role: "alert" });
  const summary = h("div", { class: "form-note" });
  const grid = h("div", { class: "cat-grid" });

  const draw = () => {
    if (!mainPicked || !chosen.has(main)) {
      // nothing picked by hand (or the pick was unticked): follow the
      // selection - except right after opening, where the main category
      // shown is the one in effect
      if (!chosen.has(main) || selectionChanged) main = highest(chosen);
      mainPicked = false;
    }
    grid.replaceChildren(
      ...categories.map((c) => {
        const check = h("input", { type: "checkbox", checked: chosen.has(c), "data-cat": c });
        check.addEventListener("change", () => {
          if (check.checked) chosen.add(c);
          else chosen.delete(c);
          selectionChanged = true;
          draw();
          grid.querySelector<HTMLInputElement>(`input[type="checkbox"][data-cat="${c}"]`)?.focus();
        });
        const radio = h("input", {
          type: "radio",
          name: "cat-main",
          checked: main === c,
          disabled: !chosen.has(c),
          "data-main": c,
          "aria-label": t("pin.mainAria", { cat: catLabel(c) }),
        });
        radio.addEventListener("change", () => {
          main = c;
          mainPicked = true;
          draw();
          grid.querySelector<HTMLInputElement>(`input[type="radio"][data-main="${c}"]`)?.focus();
        });
        return h(
          "div",
          { class: "cat-row" + (chosen.has(c) ? " on" : "") + (main === c ? " main" : "") },
          h("label", { class: "form-check" }, check, h("span", {}, catLabel(c))),
          h("label", { class: "cat-main" + (chosen.has(c) ? "" : " off") }, radio, h("span", {}, t("pin.main"))),
        );
      }),
    );
    summary.textContent = main
      ? t("pin.mainSummary", { cat: catLabel(main), group: groupLabel(state.constants.category_groups[main] ?? "") })
      : t("pin.chooseOne");
    error.textContent = "";
  };
  draw();

  const facts = [
    t("pin.autoResult", { list: m.auto_categories.map(catLabel).join(", ") }) +
      (m.judged_from_content ? t("pin.fromContent") : ""),
    t("pin.declared", { list: m.manifest_categories.join(", ") || t("common.none") }),
  ];
  if (m.evidence.length) facts.push(t("pin.evidence", { list: evidenceText(m) }));
  // what the files point to without being sure: one click selects it
  const suggested = m.suggested_category;
  const suggestion = suggested
    ? h(
        "div",
        { class: "form-note suggest" },
        h("span", {}, t("pin.suggestion", { cat: catLabel(suggested) })),
        h(
          "button",
          {
            class: "btn small",
            type: "button",
            onclick: () => {
              chosen.clear();
              chosen.add(suggested);
              main = suggested;
              mainPicked = false;
              selectionChanged = true;
              draw();
            },
          },
          t("pin.useSuggestion"),
        ),
      )
    : null;
  if (m.hidden_files > 0) {
    facts.push(
      t("pin.lockedNote", { hidden: num(m.hidden_files), named: num(m.hidden_named) }),
    );
  }
  const body = h(
    "div",
    { class: "form" },
    h(
      "div",
      {},
      t("pin.intro"),
    ),
    h("div", { class: "form-note" }, facts.join("\n")),
    suggestion,
    grid,
    summary,
    error,
  );

  type Action = "cancel" | "auto" | "apply";
  const action = await openDialog<Action>({
    title: t("pin.dialogTitle", { name: m.display_name }),
    body,
    wide: true,
    dismiss: "cancel",
    buttons: [
      { label: t("common.cancel"), value: "cancel" },
      { label: t("pin.toAuto"), value: "auto" },
      { label: t("common.apply"), value: "apply", kind: "primary" },
    ],
    beforeClose: (value) => {
      if (value === "apply" && chosen.size === 0) {
        error.textContent = t("pin.needOne");
        return false;
      }
    },
  });
  if (action === "cancel") return;
  // the main category first, the rest in the game's own order
  const tokens = action === "auto" ? [] : [main, ...categories.filter((c) => chosen.has(c) && c !== main)];
  await setOverride(m, tokens);
}

async function setOverride(m: Mod, tokens: string[]) {
  try {
    await invoke("set_category_override", { game: state.game, id: m.id, tokens });
    // categories feed both the cards and the list's group tokens
    state.mods = await invoke<Mod[]>("scan_mods", { game: state.game });
    const savedItems = JSON.parse(state.saved) as ActiveItem[];
    const resolveItems = (items: ActiveItem[]) =>
      invoke<ActiveItem[]>("resolve_items", { game: state.game, items: activeEntries(items) });
    const [active, saved] = await Promise.all([resolveItems(state.active), resolveItems(savedItems)]);
    state.active = active;
    state.saved = JSON.stringify(saved);
    await relayout();
    setStatus(
      tokens.length
        ? t("pin.done", { name: m.display_name, list: tokens.map(catLabel).join(" + ") }) +
            (tokens.length > 1 ? t("pin.doneOrder", { cat: catLabel(tokens[0]) }) : "")
        : t("pin.cleared", { name: m.display_name }),
      "ok",
    );
  } catch (e) {
    setStatus(errorText(e), "error");
  }
}

// ---------------------------------------------------------- reordering

/** Pointer-driven reordering of the load-order rows.
 *
 *  Deliberately not HTML5 drag-and-drop: inside a desktop webview that is
 *  at the mercy of the host's own drop handling, and it cannot scroll the
 *  list while dragging. Plain pointer events behave the same everywhere. */
function setupReordering() {
  const rowsEl = $("rows");
  const THRESHOLD = 5; // px of travel before a press becomes a drag
  const EDGE = 36; // px from the list edge where auto-scroll kicks in
  let drag: { from: number; x: number; y: number; active: boolean; pointerId: number } | null = null;
  let target: HTMLElement | null = null;
  let scrollTimer = 0;
  let lastY = 0;

  const mark = (row: HTMLElement | null) => {
    if (row === target) return;
    target?.classList.remove("drop-before");
    target = row;
    target?.classList.add("drop-before");
  };

  const rowAt = (x: number, y: number): HTMLElement | null => {
    const bounds = rowsEl.getBoundingClientRect();
    if (y < bounds.top || y > bounds.bottom) return null;
    // keep tracking when the pointer drifts sideways out of the list
    const cx = Math.min(Math.max(x, bounds.left + 4), bounds.right - 4);
    const hit = document.elementFromPoint(cx, y)?.closest<HTMLElement>(".row");
    return hit && rowsEl.contains(hit) ? hit : null;
  };

  const autoScroll = () => {
    const bounds = rowsEl.getBoundingClientRect();
    if (lastY < bounds.top + EDGE) rowsEl.scrollTop -= 12;
    else if (lastY > bounds.bottom - EDGE) rowsEl.scrollTop += 12;
  };

  const finish = () => {
    window.clearInterval(scrollTimer);
    scrollTimer = 0;
    rowsEl.querySelectorAll(".dragging").forEach((el) => el.classList.remove("dragging"));
    rowsEl.classList.remove("reordering");
    mark(null);
    if (drag && rowsEl.hasPointerCapture(drag.pointerId)) rowsEl.releasePointerCapture(drag.pointerId);
    drag = null;
  };

  rowsEl.addEventListener("pointerdown", (ev) => {
    if (ev.button !== 0 || (ev.target as HTMLElement).closest("button")) return;
    const row = (ev.target as HTMLElement).closest<HTMLElement>(".row.mod");
    if (!row || row.dataset.index === undefined) return;
    drag = { from: Number(row.dataset.index), x: ev.clientX, y: ev.clientY, active: false, pointerId: ev.pointerId };
  });

  rowsEl.addEventListener("pointermove", (ev) => {
    if (!drag || ev.pointerId !== drag.pointerId) return;
    if (!drag.active) {
      if (Math.hypot(ev.clientX - drag.x, ev.clientY - drag.y) < THRESHOLD) return;
      drag.active = true;
      rowsEl.setPointerCapture(ev.pointerId);
      rowsEl.classList.add("reordering");
      rowsEl.querySelector(`.row.mod[data-index="${drag.from}"]`)?.classList.add("dragging");
      scrollTimer = window.setInterval(autoScroll, 30);
    }
    lastY = ev.clientY;
    mark(rowAt(ev.clientX, ev.clientY));
  });

  rowsEl.addEventListener("pointerup", (ev) => {
    if (!drag || ev.pointerId !== drag.pointerId) return;
    const { from, active } = drag;
    const row = active ? rowAt(ev.clientX, ev.clientY) : null;
    finish();
    if (row?.dataset.insertAt !== undefined) void moveActive(from, Number(row.dataset.insertAt));
  });

  rowsEl.addEventListener("pointercancel", finish);
  document.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape" && drag) finish();
  });

  // keyboard: Alt+Up / Alt+Down moves the focused row by one place
  rowsEl.addEventListener("keydown", (ev) => {
    if (!ev.altKey || (ev.key !== "ArrowUp" && ev.key !== "ArrowDown")) return;
    const row = (ev.target as HTMLElement).closest<HTMLElement>(".row.mod");
    if (!row || row.dataset.index === undefined) return;
    ev.preventDefault();
    const from = Number(row.dataset.index);
    const to = ev.key === "ArrowUp" ? from - 1 : from + 1;
    if (to < 0 || to >= state.active.length) return;
    const name = state.active[from].name;
    // moving down by one means inserting after the next row
    void moveActive(from, ev.key === "ArrowUp" ? to : to + 1).then(() => {
      const moved = state.active.findIndex((a) => a.name === name);
      rowsEl.querySelector<HTMLElement>(`.row.mod[data-index="${moved}"]`)?.focus();
    });
  });
}

// ------------------------------------------------------------- managing

function fmtSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

/** `2026-11-14_23-51-57[_1]` -> `2026-11-14 23:51:57`. */
function fmtStamp(stamp: string): string {
  const m = /^(\d{4}-\d{2}-\d{2})_(\d{2})-(\d{2})-(\d{2})/.exec(stamp);
  return m ? `${m[1]} ${m[2]}:${m[3]}:${m[4]}` : stamp;
}

/** Re-resolve both the edited and the saved list against the current scan
 *  (after a mod disappeared), so the dirty check stays meaningful. */
async function resolveLists() {
  const savedItems = JSON.parse(state.saved) as ActiveItem[];
  const resolveItems = (items: ActiveItem[]) =>
    invoke<ActiveItem[]>("resolve_items", { game: state.game, items: activeEntries(items) });
  const [active, saved] = await Promise.all([resolveItems(state.active), resolveItems(savedItems)]);
  state.active = active;
  state.saved = JSON.stringify(saved);
  await relayout();
}

async function toggleFavorite(m: Mod) {
  try {
    await invoke("set_favorite", { game: state.game, id: m.id, favorite: !m.favorite });
    m.favorite = !m.favorite;
    renderLibrary();
  } catch (e) {
    setStatus(errorText(e), "error");
  }
}

type DetailAction = "close" | "reveal" | "workshop" | "delete" | "extract";

/** Archive formats the extractor can walk. A mod that is already a folder
 *  has nothing to extract. */
const EXTRACTABLE = new Set(["zip", "hashfs_v1", "hashfs_v2"]);

async function openDetails(m: Mod) {
  const icon = icons.get(`${state.game}/${m.id}`);
  const rows: [string, string][] = [
    [t("detail.author"), m.author || "-"],
    [t("detail.version"), m.version || "-"],
    [
      t("detail.category"),
      m.categories.map(catLabel).join(", ") +
        (m.category_pins.length ? t("detail.pinned") : m.judged_from_content ? t("detail.fromContent") : ""),
    ],
    [t("detail.declared"), m.manifest_categories.join(", ") || "-"],
    ...(m.suggested_category
      ? [[t("detail.suggestion"), catLabel(m.suggested_category)] as [string, string]]
      : []),
    ...(m.evidence.length ? [[t("detail.evidence"), evidenceText(m)] as [string, string]] : []),
    [t("detail.source"), m.source === "workshop" ? t("detail.sourceWorkshop") : t("detail.sourceLocal")],
    [t("detail.format"), m.format],
    [t("detail.compat"), m.compatible_versions.join(", ") || t("detail.compatNone")],
    [t("detail.installed"), m.installed_at ? new Date(m.installed_at * 1000).toLocaleString(locale()) : "-"],
    [t("detail.file"), m.path],
  ];
  if (m.hidden_files > 0) {
    rows.push([
      t("detail.listing"),
      t("detail.listingHidden", { hidden: num(m.hidden_files), named: num(m.hidden_named) }),
    ]);
  }
  if (m.error) rows.push([t("detail.readError"), m.error]);
  const error = h("div", { class: "form-error", role: "alert" });
  const body = h(
    "div",
    { class: "detail" },
    icon ? h("img", { class: "detail-icon", src: icon, alt: "" }) : null,
    h("dl", {}, ...rows.flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)])),
    m.description ? h("div", { class: "detail-desc" }, m.description) : null,
    error,
  );
  const buttons: { label: string; value: DetailAction; kind?: "primary" | "danger" }[] = [
    { label: t("common.close"), value: "close" },
    { label: t("detail.reveal"), value: "reveal" },
  ];
  if (EXTRACTABLE.has(m.format) && !m.error) buttons.push({ label: t("detail.extract"), value: "extract" });
  if (m.source === "workshop") buttons.push({ label: t("detail.workshopPage"), value: "workshop" });
  else buttons.push({ label: t("detail.trash"), value: "delete", kind: "danger" });

  const action = await openDialog<DetailAction>({
    title: m.display_name,
    body,
    wide: true,
    dismiss: "close",
    buttons,
    beforeClose: async (value) => {
      if (value !== "reveal" && value !== "workshop") return;
      try {
        await invoke(value === "reveal" ? "reveal_mod" : "open_workshop_page", { game: state.game, id: m.id });
      } catch (e) {
        error.textContent = errorText(e);
      }
      return false; // these act on the mod and keep the dialog open
    },
  });
  if (action === "delete") await deleteMod(m);
  else if (action === "extract") await extractMod(m);
}

/** Ask where to, then unpack the mod's archive into a new folder there. */
async function extractMod(m: Mod) {
  try {
    const parent = await pickOpen({
      directory: true,
      multiple: false,
      title: t("extract.pickTitle", { name: m.display_name }),
    });
    if (typeof parent === "string") await extractModTo(m.id, parent);
  } catch (e) {
    setStatus(errorText(e), "error");
  }
}

async function extractModTo(id: string, parent: string) {
  const m = state.mods.find((x) => x.id === id);
  if (!m) throw new Error(t("err.not_scanned"));
  const game = state.game;

  const bar = h("progress", { class: "extract-bar", max: "1", value: "0" });
  const count = h("div", { class: "form-note" }, t("extract.opening"));
  const body = h(
    "div",
    { class: "form" },
    h("div", {}, t("extract.working", { name: m.display_name })),
    bar,
    count,
  );
  const onProgress = new Channel<{ done: number; total: number }>();
  onProgress.onmessage = ({ done, total }) => {
    bar.max = Math.max(total, 1);
    bar.value = done;
    count.textContent = t("extract.progress", { done: num(done), total: num(total) });
  };

  // closing the progress dialog in any way (button, Escape, backdrop) is
  // a request to stop; the dialog goes away on its own when the run ends
  let running = true;
  let dialogOpen = true;
  void openDialog<void>({
    title: t("extract.title"),
    body,
    dismiss: undefined,
    buttons: [{ label: t("common.cancel"), value: undefined }],
  }).then(() => {
    dialogOpen = false;
    if (running) {
      setStatus(t("extract.stopping"));
      invoke("cancel_extract").catch(() => {});
    }
  });

  let r: ExtractOutcome;
  try {
    r = await invoke<ExtractOutcome>("extract_mod", {
      game,
      id,
      destParent: parent,
      folderName: m.display_name,
      onProgress,
    });
  } catch (e) {
    running = false;
    if (dialogOpen) dismissDialog();
    setStatus(t("extract.failed", { error: errorText(e) }), "error");
    return;
  }
  running = false;
  if (dialogOpen) dismissDialog();

  const n = num;
  const lines: string[] = [];
  if (r.cancelled) lines.push(t("extract.cancelledKept", { n: n(r.extracted) }));
  else if (r.total === 0) lines.push(t("extract.nothing"));
  else lines.push(t("extract.summary", { total: n(r.total), extracted: n(r.extracted) }));
  if (r.dest) lines.push(t("extract.location", { path: r.dest }));
  if (r.recovered + r.unnamed > 0) {
    lines.push(
      "",
      t("extract.locked"),
      t("extract.recovered", { n: n(r.recovered) }),
      t("extract.unnamed", { n: n(r.unnamed) }) +
        (r.unnamed ? t("extract.unnamedNote") : ""),
    );
  }
  if (r.failed > 0) {
    lines.push("", t("extract.failedList", { n: n(r.failed) }));
    lines.push(...r.failures.slice(0, 20).map((f) => `  ${f.entry}  (${f.error})`));
    if (r.failed > 20) lines.push(t("common.more", { n: n(r.failed - 20) }));
  }
  const ok = !r.cancelled && r.failed === 0 && r.extracted > 0;
  setStatus(
    r.cancelled
      ? t("extract.cancelled")
      : t("extract.status", { name: m.display_name, n: n(r.extracted) }) + (r.failed ? t("extract.statusFailed", { n: n(r.failed) }) : ""),
    ok ? "ok" : r.cancelled ? "info" : "error",
  );

  const error = h("div", { class: "form-error", role: "alert" });
  const buttons: { label: string; value: "close" | "open"; kind?: "primary" }[] = [{ label: t("common.close"), value: "close" }];
  if (r.dest) buttons.push({ label: t("extract.openFolder"), value: "open", kind: "primary" });
  await openDialog<"close" | "open">({
    title: r.cancelled ? t("extract.cancelledTitle") : t("extract.doneTitle"),
    body: h("div", { class: "form" }, h("div", { class: "report" }, lines.join("\n")), error),
    wide: true,
    dismiss: "close",
    buttons,
    beforeClose: async (value) => {
      if (value !== "open") return;
      try {
        await invoke("open_extracted");
      } catch (e) {
        error.textContent = errorText(e);
        return false;
      }
    },
  });
}

async function deleteMod(m: Mod) {
  const game = state.game;
  let users: string[] = [];
  try {
    users = (await invoke<Record<string, string[]>>("mod_usage", { game, ids: [m.id] }))[m.id] ?? [];
  } catch (e) {
    setStatus(errorText(e), "error");
    return;
  }
  const ok = await confirmModal(
    t("delete.title"),
    t("delete.body", { name: m.display_name }) +
      (users.length
        ? t("delete.inUse", { profiles: users.join(", ") })
        : ""),
    t("delete.ok"),
    true,
  );
  if (!ok) return;
  try {
    const r = await invoke<DeleteResult>("delete_mods", { game, ids: [m.id] });
    if (r.deleted.length === 0) {
      setStatus(t("delete.failed", { error: r.failed[0] ? backendText(r.failed[0].error) : t("common.unknownError") }), "error");
      return;
    }
    state.mods = state.mods.filter((x) => !r.deleted.includes(x.id));
    await resolveLists();
    setStatus(t("delete.done", { name: m.display_name }), "ok");
  } catch (e) {
    setStatus(errorText(e), "error");
  }
}

async function openBackups() {
  const game = state.game;
  const profilePath = state.profilePath;
  if (!profilePath) {
    setStatus(t("backup.noProfile"), "error");
    return;
  }
  const profileName = state.profiles.find((p) => p.path === profilePath)?.profile_name ?? "";
  const list = h("div", { class: "backup-list" });
  const footer = h("div", { class: "form-note" });
  const error = h("div", { class: "form-error", role: "alert" });
  let restore: BackupDto | null = null;

  const refresh = async () => {
    const backups = await invoke<BackupDto[]>("list_backups", { game, profilePath });
    list.replaceChildren(
      ...(backups.length
        ? backups.map((b) => {
            const del = h("button", { class: "btn small" }, t("common.delete"));
            // deleting a backup cannot be undone: ask for a second click
            del.addEventListener("click", async () => {
              if (del.dataset.armed !== "1") {
                del.dataset.armed = "1";
                del.textContent = t("backup.reallyDelete");
                del.classList.add("danger");
                return;
              }
              try {
                await invoke("delete_backup", { game, profilePath, backupPath: b.path });
                await refresh();
              } catch (e) {
                error.textContent = errorText(e);
              }
            });
            return h(
              "div",
              { class: "backup-row" },
              h("span", { class: "backup-time" }, fmtStamp(b.timestamp)),
              h("span", { class: "muted" }, fmtSize(b.size_bytes)),
              h(
                "button",
                {
                  class: "btn small",
                  onclick: () => {
                    restore = b;
                    dismissDialog();
                  },
                },
                t("common.restore"),
              ),
              del,
            );
          })
        : [h("div", { class: "empty" }, t("backup.empty"))]),
    );
    const total = backups.reduce((sum, b) => sum + b.size_bytes, 0);
    const keep = state.settings?.settings.backup_keep ?? 0;
    footer.textContent =
      t("backup.total", { n: backups.length, size: fmtSize(total) }) +
      (keep ? t("backup.keepN", { n: keep }) : t("backup.keepAll")) +
      (state.settings ? t("backup.location", { path: state.settings.backup_root }) : "");
  };

  try {
    await refresh();
  } catch (e) {
    setStatus(errorText(e), "error");
    return;
  }
  await openDialog<void>({
    title: t("backup.title", { name: profileName }),
    body: h("div", { class: "form" }, list, footer, error),
    wide: true,
    dismiss: undefined,
    buttons: [{ label: t("common.close"), value: undefined, kind: "primary" }],
  });
  const chosen = restore as BackupDto | null;
  if (!chosen) return;

  const ok = await confirmModal(
    t("restore.title"),
    t("restore.body", { when: fmtStamp(chosen.timestamp), name: profileName }) +
      t("restore.note") +
      (isDirty() ? t("restore.dirty") : "") +
      t("restore.closeGame", { game: game.toUpperCase() }),
    t("common.restore"),
    true,
  );
  if (!ok) return;
  try {
    const safety = await invoke<string>("restore_backup", { game, profilePath, backupPath: chosen.path });
    state.profiles = await invoke<Profile[]>("list_profiles", { game });
    renderProfileSelect();
    await loadProfile();
    setStatus(t("restore.done", { path: safety }), "ok");
  } catch (e) {
    setStatus(t("restore.failed", { error: errorText(e) }), "error");
  }
}

async function openSettings() {
  let view: SettingsView;
  try {
    view = await invoke<SettingsView>("get_settings");
  } catch (e) {
    setStatus(errorText(e), "error");
    return;
  }
  state.settings = view;
  const s = view.settings;

  const lookup = h("input", { type: "checkbox", id: "set-lookup", checked: s.workshop_lookup });
  // each language under its own name, so it can be found in any language
  const language = h(
    "select",
    { id: "set-language" },
    h("option", { value: "", selected: !s.language }, t("settings.languageSystem")),
    ...LANGS.map((l) => h("option", { value: l.id, selected: s.language === l.id }, l.name)),
  );
  const keep = h("input", {
    type: "number",
    id: "set-keep",
    min: "0",
    max: "1000",
    value: String(s.backup_keep),
  });
  const pathInput = (id: string, value: string | null, detected: string | null | undefined) =>
    h("input", {
      type: "text",
      id,
      value: value ?? "",
      spellcheck: "false",
      placeholder: detected ? t("settings.detected", { path: detected }) : t("settings.detectFailed"),
    });
  const inputs = {
    ets2: {
      documents: pathInput("set-ets2-docs", s.ets2.documents_dir, view.detected.ets2?.documents_dir),
      workshop: pathInput("set-ets2-ws", s.ets2.workshop_dir, view.detected.ets2?.workshop_dir),
    },
    ats: {
      documents: pathInput("set-ats-docs", s.ats.documents_dir, view.detected.ats?.documents_dir),
      workshop: pathInput("set-ats-ws", s.ats.workshop_dir, view.detected.ats?.workshop_dir),
    },
  };
  const names = h("textarea", { id: "set-mapbase", rows: "5", spellcheck: "false" });
  names.value = s.map_base_names.join("\n");
  const error = h("div", { class: "form-error", role: "alert" });
  const field = (label: string, control: HTMLElement, note?: string) =>
    h(
      "label",
      { class: "form-row" },
      h("span", { class: "form-label" }, label),
      control,
      note ? h("span", { class: "form-note" }, note) : null,
    );
  const pathFields = (title: string, group: { documents: HTMLElement; workshop: HTMLElement }) => [
    h("h3", {}, title),
    field(t("settings.docsDir"), group.documents, t("settings.docsNote")),
    field(t("settings.wsDir"), group.workshop, t("settings.wsNote")),
  ];
  const body = h(
    "div",
    { class: "form" },
    field(t("settings.language"), language),
    h(
      "label",
      { class: "form-check" },
      lookup,
      h("span", {}, t("settings.lookup")),
    ),
    h("div", { class: "form-note" }, t("settings.lookupNote")),
    field(t("settings.keep"), keep, t("settings.keepNote", { path: view.backup_root })),
    ...pathFields(t("settings.ets2Paths"), inputs.ets2),
    ...pathFields(t("settings.atsPaths"), inputs.ats),
    h("h3", {}, t("settings.mapBase")),
    field(
      t("settings.mapBaseNames"),
      names,
      t("settings.mapBaseNote", { group: groupLabel("map_base") }),
    ),
    error,
  );

  const text = (el: HTMLInputElement) => el.value.trim() || null;
  const saved = await openDialog<boolean>({
    title: t("top.settings"),
    body,
    wide: true,
    dismiss: false,
    buttons: [
      { label: t("common.cancel"), value: false },
      { label: t("common.save"), value: true, kind: "primary" },
    ],
    beforeClose: async (value) => {
      if (!value) return;
      const count = Number(keep.value);
      if (!Number.isInteger(count) || count < 0 || count > 1000) {
        error.textContent = t("settings.keepInvalid");
        return false;
      }
      const next: Settings = {
        ...s,
        language: language.value || null,
        workshop_lookup: lookup.checked,
        backup_keep: count,
        map_base_names: names.value.split("\n"),
        ets2: { ...s.ets2, documents_dir: text(inputs.ets2.documents), workshop_dir: text(inputs.ets2.workshop) },
        ats: { ...s.ats, documents_dir: text(inputs.ats.documents), workshop_dir: text(inputs.ats.workshop) },
      };
      try {
        await invoke("save_settings", { settings: next });
      } catch (e) {
        error.textContent = errorText(e);
        return false;
      }
    },
  });
  if (!saved) return;

  try {
    [state.settings, state.installs] = await Promise.all([
      invoke<SettingsView>("get_settings"),
      invoke<InstallInfo[]>("get_installs"),
    ]);
  } catch (e) {
    setStatus(errorText(e), "error");
    return;
  }
  applyLanguage();
  if (isDirty()) {
    renderGameSwitch();
    setStatus(t("settings.savedDirty"), "ok");
    return;
  }
  const target = state.installs.find((i) => i.game === state.game) ?? state.installs[0];
  if (!target) {
    renderGameSwitch();
    setStatus(t("settings.savedNoInstall"), "error");
    return;
  }
  icons.clear();
  await loadGame(target.game);
}

// -------------------------------------------------------------- sharing

type ShareStatus = "found" | "missing_workshop" | "missing_local";

interface ShareCheck {
  name: string;
  display_name: string;
  status: ShareStatus;
  workshop_url: string | null;
  share_version: string;
  local_version: string | null;
  outdated: boolean;
}

interface SharePreview {
  kind: "modshare" | "profile";
  profile_name: string;
  entries: ShareCheck[];
}

interface MapComboPreview {
  count: number;
  missing: string[];
  outdated: { name: string; local: string; combo: string }[];
  order: string[];
}

const JSON_FILTER = [{ name: "JSON", extensions: ["json"] }];

function currentProfileName(): string {
  return state.profiles.find((p) => p.path === state.profilePath)?.profile_name ?? "";
}

/** File name safe on every platform, from a profile name. */
function fileStem(name: string): string {
  return name.replace(/[\\/:*?"<>|\x00-\x1f]/g, "_").trim() || "load-order";
}

async function exportShareTo(path: string) {
  try {
    const count = await invoke<number>("export_share", {
      game: state.game,
      profileName: currentProfileName(),
      items: activeEntries(),
      path,
    });
    setStatus(t("share.exported", { n: count, path }), "ok");
  } catch (e) {
    setStatus(t("share.exportFailed", { error: errorText(e) }), "error");
  }
}

/** Preview a share file (or a profile.sii) and, on the user's go-ahead,
 *  replace the edited list with it. Nothing is written until Save. */
async function importShareFrom(path: string) {
  let preview: SharePreview;
  try {
    preview = await invoke<SharePreview>("preview_share", { game: state.game, path });
  } catch (e) {
    setStatus(t("share.importFailed", { error: errorText(e) }), "error");
    return;
  }
  const missing = preview.entries.filter((e) => e.status !== "found");
  const outdated = preview.entries.filter((e) => e.outdated);
  const rows = preview.entries.map((e) =>
    h(
      "div",
      { class: "share-row" + (e.status === "found" ? "" : " missing") },
      h("span", { class: "row-name" }, e.display_name || e.name),
      e.outdated
        ? h("span", { class: "badge warn", title: t("share.versions", { local: e.local_version ?? "", share: e.share_version }) }, t("badge.outdated"))
        : null,
      e.status === "missing_local" ? h("span", { class: "badge err" }, t("share.missingLocal")) : null,
      e.status === "missing_workshop"
        ? h(
            "button",
            {
              class: "btn small",
              title: t("share.openWorkshop"),
              onclick: () => {
                const id = e.workshop_url?.split("id=")[1] ?? "";
                invoke("open_workshop_item", { workshopId: id }).catch((err) => setStatus(errorText(err), "error"));
              },
            },
            t("share.missingWorkshop"),
          )
        : null,
    ),
  );
  const source =
    preview.kind === "profile"
      ? preview.profile_name
        ? t("share.fromProfileNamed", { name: preview.profile_name })
        : t("share.fromProfile")
      : preview.profile_name
        ? t("share.fromFileNamed", { name: preview.profile_name })
        : t("share.fromFile");
  const summary =
    t("share.summary", { source, n: preview.entries.length, installed: preview.entries.length - missing.length }) +
    (missing.length ? t("share.summaryMissing", { n: missing.length }) : "") +
    (outdated.length ? t("share.summaryOutdated", { n: outdated.length }) : "");
  const body = h(
    "div",
    { class: "form" },
    h("div", {}, summary),
    h(
      "div",
      { class: "form-note" },
      t("share.importNote"),
    ),
    h("div", { class: "share-list" }, ...rows),
  );
  type Choice = "cancel" | "installed" | "all";
  const buttons: { label: string; value: Choice; kind?: "primary" }[] = [{ label: t("common.cancel"), value: "cancel" }];
  if (missing.length) {
    buttons.push({ label: t("share.importInstalled"), value: "installed", kind: "primary" });
    buttons.push({ label: t("share.importEverything"), value: "all" });
  } else {
    buttons.push({ label: t("share.import"), value: "all", kind: "primary" });
  }
  const choice = await openDialog<Choice>({ title: t("share.importTitle"), body, wide: true, dismiss: "cancel", buttons });
  if (choice === "cancel") return;

  const picked = preview.entries.filter((e) => choice === "all" || e.status === "found");
  try {
    state.active = await invoke<ActiveItem[]>("resolve_items", {
      game: state.game,
      items: picked.map(({ name, display_name }) => ({ name, display_name })),
    });
    await relayout();
    setStatus(t("share.imported", { n: picked.length }), "ok");
  } catch (e) {
    setStatus(t("share.importFailed", { error: errorText(e) }), "error");
  }
}

async function exportMapComboTo(path: string) {
  try {
    const count = await invoke<number>("export_map_combo", { game: state.game, items: activeEntries(), path });
    setStatus(t("combo.exported", { n: count, path }), "ok");
  } catch (e) {
    setStatus(t("share.exportFailed", { error: errorText(e) }), "error");
  }
}

async function importMapComboFrom(path: string) {
  let preview: MapComboPreview;
  try {
    preview = await invoke<MapComboPreview>("preview_map_combo", { game: state.game, items: activeEntries(), path });
  } catch (e) {
    setStatus(t("share.importFailed", { error: errorText(e) }), "error");
    return;
  }
  if (preview.missing.length) {
    await infoModal(
      t("combo.cannotTitle"),
      t("combo.cannotBody") +
        preview.missing.map((n) => `- ${n}`).join("\n"),
    );
    return;
  }
  const byName = new Map(state.active.map((a) => [a.name, a]));
  state.active = preview.order.flatMap((n) => byName.get(n) ?? []);
  await relayout();
  setStatus(t("combo.applied", { n: preview.count }), "ok");
  if (preview.outdated.length) {
    await infoModal(
      t("combo.outdatedTitle"),
      t("combo.outdatedBody") +
        preview.outdated.map((o) => t("combo.outdatedLine", { name: o.name, local: o.local, combo: o.combo })).join("\n"),
    );
  }
}

type ShareAction = "close" | "export" | "import" | "combo-export" | "combo-import";

async function openShareMenu() {
  const body = h(
    "div",
    { class: "form" },
    h("div", {}, t("share.intro")),
    h(
      "div",
      { class: "form-note" },
      t("share.note"),
    ),
  );
  const action = await openDialog<ShareAction>({
    title: t("order.share"),
    body,
    wide: true,
    dismiss: "close",
    buttons: [
      { label: t("common.close"), value: "close" },
      { label: t("share.exportAll"), value: "export", kind: "primary" },
      { label: t("share.importAll"), value: "import" },
      { label: t("combo.export"), value: "combo-export" },
      { label: t("combo.import"), value: "combo-import" },
    ],
  });
  try {
    if (action === "export") {
      const path = await pickSave({ defaultPath: `${fileStem(currentProfileName())}.modshare.json`, filters: JSON_FILTER });
      if (path) await exportShareTo(path);
    } else if (action === "import") {
      const path = await pickOpen({
        multiple: false,
        filters: [{ name: t("share.fileFilter"), extensions: ["json", "sii"] }],
      });
      if (typeof path === "string") await importShareFrom(path);
    } else if (action === "combo-export") {
      const path = await pickSave({ defaultPath: `${fileStem(currentProfileName())}.mapcombo.json`, filters: JSON_FILTER });
      if (path) await exportMapComboTo(path);
    } else if (action === "combo-import") {
      const path = await pickOpen({ multiple: false, filters: JSON_FILTER });
      if (typeof path === "string") await importMapComboFrom(path);
    }
  } catch (e) {
    setStatus(errorText(e), "error");
  }
}

// The same entry points without the native file picker, so automated UI
// checks can drive the flows with a path of their own.
Object.assign(window, {
  __scsmm: { exportShareTo, importShareFrom, exportMapComboTo, importMapComboFrom, extractModTo },
});

// ----------------------------------------------------------------- boot

/** Store the library's sort order and filters for the next start (best
 *  effort). The search text is left out on purpose. */
function rememberView() {
  const { sort, category, source, favorites } = state.filter;
  const view: ViewSettings = { sort, category, source, favorites };
  if (state.settings) state.settings.settings.view = view;
  invoke("remember_view", { view }).catch(() => {});
}

/** Put the stored sort order and filters back, on the controls too. */
function restoreView(view: ViewSettings | undefined) {
  if (!view) return;
  const filter = state.filter;
  const pick = (id: string, wanted: string, fallback: string) => {
    const select = $<HTMLSelectElement>(id);
    select.value = wanted;
    // a value the control has no option for must not filter invisibly
    if (select.value !== wanted) select.value = fallback;
    return select.value;
  };
  filter.sort = pick("sort", view.sort, "name");
  filter.category = pick("filter-category", view.category, "");
  filter.source = pick("filter-source", view.source, "");
  filter.favorites = view.favorites;
  $<HTMLInputElement>("filter-favorites").checked = view.favorites;
}

function bindControls() {
  const filter = state.filter;
  $<HTMLInputElement>("search").addEventListener("input", (ev) => {
    filter.q = (ev.target as HTMLInputElement).value;
    renderLibrary();
  });
  const bindSelect = (id: string, key: "category" | "source" | "sort") =>
    $<HTMLSelectElement>(id).addEventListener("change", (ev) => {
      filter[key] = (ev.target as HTMLSelectElement).value;
      renderLibrary();
      rememberView();
    });
  bindSelect("filter-category", "category");
  bindSelect("filter-source", "source");
  bindSelect("sort", "sort");
  $<HTMLInputElement>("filter-favorites").addEventListener("change", (ev) => {
    filter.favorites = (ev.target as HTMLInputElement).checked;
    renderLibrary();
    rememberView();
  });
  $("open-settings").addEventListener("click", () => void openSettings());
  $("open-backups").addEventListener("click", () => void openBackups());
  $("open-share").addEventListener("click", () => void openShareMenu());

  $<HTMLSelectElement>("profile-select").addEventListener("change", async (ev) => {
    const select = ev.target as HTMLSelectElement;
    if (isDirty() && !(await confirmDiscard())) {
      select.value = state.profilePath;
      return;
    }
    state.profilePath = select.value;
    try {
      await loadProfile();
      rememberPlace();
    } catch (e) {
      setStatus(errorText(e), "error");
    }
  });

  $("rescan").addEventListener("click", async () => {
    if (isDirty() && !(await confirmDiscard())) return;
    icons.clear();
    await loadGame(state.game);
  });
  $("auto-sort").addEventListener("click", () => void autoSort());
  $("show-conflicts").addEventListener("click", () => void infoModal(t("conflict.title"), conflictReport()));
  $("revert").addEventListener("click", () => void revert());
  $("save").addEventListener("click", () => void save());
}

/** Switch the interface to the stored language (or the system's) and
 *  redraw everything that carries text. */
function applyLanguage() {
  setLang(resolveLang(state.settings?.settings.language));
  applyStatic();
  const select = $<HTMLSelectElement>("filter-category");
  const chosen = select.value;
  select.replaceChildren(
    h("option", { value: "" }, t("lib.allCategories")),
    ...state.constants.categories.map((c) => h("option", { value: c }, catLabel(c))),
  );
  select.value = chosen;
  renderGameSwitch();
  renderProfileSelect();
  renderAll();
}

async function boot() {
  // the system's language until the settings say otherwise
  applyStatic();
  bindControls();
  setupReordering();
  try {
    [state.constants, state.installs, state.settings] = await Promise.all([
      invoke<Constants>("constants"),
      invoke<InstallInfo[]>("get_installs"),
      invoke<SettingsView>("get_settings"),
    ]);
  } catch (e) {
    setStatus(errorText(e), "error");
    return;
  }
  applyLanguage();
  restoreView(state.settings.settings.view);
  if (state.installs.length === 0) {
    renderGameSwitch();
    renderAll();
    setStatus(t("status.noInstall"), "error");
    return;
  }
  // start where the user left off, else ETS2, else whatever exists
  const lastGame = state.settings.settings.last_game;
  const first =
    state.installs.find((i) => i.game === lastGame) ??
    state.installs.find((i) => i.game === "ets2") ??
    state.installs[0];
  await loadGame(first.game);
}

void boot();
