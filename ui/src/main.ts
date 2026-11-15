import { Channel, invoke } from "@tauri-apps/api/core";
import { open as pickOpen, save as pickSave } from "@tauri-apps/plugin-dialog";
import { $, type DialogButton, confirmModal, dismissDialog, h, infoModal, openDialog } from "./dom";
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

/** How one active mod hangs together with others, by active name. A mod
 *  that overwrites a file which only another mod brings was made for it. */
interface ModRelations {
  /** Mods above it that overwrite files only it brings: its patches and add-ons. */
  patches: string[];
  /** Mods below it whose own files it overwrites: what it was made for. */
  bases: string[];
  /** Everything that builds on it, directly or through another patch. */
  dependents: string[];
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
  /** Ask GitHub at start whether a newer version is out. */
  update_check: boolean;
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
  /** Version of the running app. */
  version: string;
}

/** What the last look at the releases found. */
interface UpdateInfo {
  current: string;
  latest: string;
  newer: boolean;
  /** The app can replace itself; otherwise only the release page helps. */
  installable: boolean;
  /** Bytes to download. */
  size: number;
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

/** A named load order of the game, stored by the app. */
interface Preset {
  name: string;
  /** Display order: the top of the load order first. */
  mods: { name: string; display_name: string }[];
}

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
  /** Patches and bases of the active mods that have any. */
  relations: {} as Record<string, ModRelations>,
  plan: NO_PLAN,
  /** Presets of the current game. */
  presets: [] as Preset[],
  /** Name of the preset the list was last taken from or stored as, to
   *  offer storing later edits back into it. */
  basePreset: null as string | null,
  /** Rows picked in the load order. Held by identity: a list that is
   *  loaded anew starts without a selection, a reordered one keeps it. */
  selection: new Set<ActiveItem>(),
  /** Where a Shift+click range starts. */
  anchor: null as ActiveItem | null,
  /** Mods copied from a load order, to paste into another one. */
  clipboard: null as { game: GameId; mods: Preset["mods"] } | null,
  filter: { q: "", category: "", source: "", sort: "name", favorites: false },
  settings: null as SettingsView | null,
  /** The latest release, once it has been looked up. */
  update: null as UpdateInfo | null,
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
  state.relations = {};
  state.plan = NO_PLAN;
  state.presets = [];
  state.basePreset = null;
  renderGameSwitch();
  renderProfileSelect();
  renderAll();
  setStatus(t("status.scanning"));
  const started = performance.now();
  try {
    const [mods, profiles, presets] = await Promise.all([
      invoke<Mod[]>("scan_mods", { game }),
      invoke<Profile[]>("list_profiles", { game }),
      // presets are an extra: without them the game still loads
      invoke<Preset[]>("list_presets", { game }).catch(() => [] as Preset[]),
    ]);
    if (epoch !== state.epoch) return;
    state.mods = mods;
    state.profiles = profiles;
    state.presets = presets;
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
  state.basePreset = null;
  await relayout();
}

/** The list as the backend wants it: identity only, display order. */
function activeEntries(items: ActiveItem[] = state.active) {
  return items.map(({ name, display_name }) => ({ name, display_name }));
}

/** Recompute group headers and conflicts for the current list and redraw. */
async function relayout() {
  const epoch = state.epoch;
  const [layout, conflicts, relations, plan] = await Promise.all([
    invoke<Layout>("layout_rows", { tokens: state.active.map((a) => a.token) }),
    invoke<ConflictReport>("analyze_conflicts", { game: state.game, items: activeEntries() }),
    invoke<Record<string, ModRelations>>("mod_relations", { game: state.game, items: activeEntries() }),
    invoke<SortPlan>("sort_plan", { game: state.game, items: activeEntries() }),
  ]);
  if (epoch !== state.epoch) return;
  state.layout = layout;
  state.conflicts = conflicts;
  state.relations = relations;
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
  renderPresetSelect();
  // rows that left the list are no longer picked
  const listed = new Set(state.active);
  for (const item of state.selection) if (!listed.has(item)) state.selection.delete(item);
  renderCopyPaste();

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
  paintRelations();
}

function orderRow(row: Extract<Row, { kind: "mod" }>, item: ActiveItem): HTMLElement {
  return h(
    "div",
    {
      class:
        "row mod" +
        (row.misplaced && !state.plan.held.includes(item.name) ? " misplaced" : "") +
        (item.installed ? "" : " missing") +
        (state.selection.has(item) ? " selected" : ""),
      "aria-selected": String(state.selection.has(item)),
      tabindex: "0",
      "data-index": String(row.index),
    },
    h("span", { class: "grip", "aria-hidden": "true" }, "⋮⋮"),
    h("span", { class: "row-name" }, item.display_name || item.name),
    // filled in by paintRelations for the rows that belong to a picked one
    h("span", { class: "badge rel", hidden: true }),
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

/** Take a mod out of the load order. Where other active mods look like
 *  patches or add-ons made for it, ask first whether they go with it. */
async function removeActive(name: string) {
  const item = state.active.find((a) => a.name === name);
  if (!item) return;
  const active = activeNames();
  const dependents = (state.relations[name]?.dependents ?? []).filter((n) => active.has(n));
  const gone = new Set([name]);
  if (dependents.length) {
    const choice = await askAboutDependents(item, dependents);
    if (choice === "cancel") return;
    if (choice === "all") for (const n of dependents) gone.add(n);
  }
  state.active = state.active.filter((a) => !gone.has(a.name));
  await relayout();
  if (gone.size > 1) {
    setStatus(
      t("remove.doneAll", { name: item.display_name || item.name, n: gone.size - 1 }) + t("sort.saveNote"),
      "ok",
    );
  }
}

/** The mods that build on `item` are about to lose their base: let the
 *  user take them out too, take out only `item`, or leave things alone. */
function askAboutDependents(item: ActiveItem, dependents: string[]): Promise<"cancel" | "only" | "all"> {
  const body = h(
    "div",
    { class: "form" },
    h("div", {}, t("remove.body", { n: dependents.length })),
    h("ul", { class: "name-list" }, ...dependents.map((n) => h("li", {}, displayNameOf(n)))),
    h("div", { class: "form-note" }, t("remove.note")),
  );
  return openDialog<"cancel" | "only" | "all">({
    title: t("remove.title", { name: item.display_name || item.name }),
    body,
    dismiss: "cancel",
    buttons: [
      { label: t("common.cancel"), value: "cancel" },
      { label: t("remove.all", { n: dependents.length }), value: "all", kind: "primary" },
      { label: t("remove.only"), value: "only" },
    ],
  });
}

async function moveActive(from: number, insertAt: number) {
  if (from < 0 || from >= state.active.length) return;
  const target = insertAt > from ? insertAt - 1 : insertAt;
  if (target === from) return;
  const [item] = state.active.splice(from, 1);
  state.active.splice(target, 0, item);
  await relayout();
}

// ------------------------------------------------- selecting and copying

/** Show the selection on the rows as they are, without redrawing them (a
 *  redraw would take the focus the keyboard shortcuts rely on). */
function paintSelection() {
  for (const row of $("rows").querySelectorAll<HTMLElement>(".row.mod")) {
    const picked = state.selection.has(state.active[Number(row.dataset.index)]);
    row.classList.toggle("selected", picked);
    row.setAttribute("aria-selected", String(picked));
  }
  paintRelations();
  renderCopyPaste();
}

type Relation = "patch" | "base" | "both";

/** What each mod is to the picked ones: a patch of one of them, the base
 *  of one, or both. The picked mods themselves are not in it. */
function relatedToSelection(): Map<string, Relation> {
  const out = new Map<string, Relation>();
  const mark = (name: string, kind: "patch" | "base") => {
    const had = out.get(name);
    out.set(name, had && had !== kind ? "both" : kind);
  };
  for (const item of state.selection) {
    const relations = state.relations[item.name];
    for (const name of relations?.patches ?? []) mark(name, "patch");
    for (const name of relations?.bases ?? []) mark(name, "base");
  }
  for (const item of state.selection) out.delete(item.name);
  return out;
}

/** Highlight the rows that belong to the picked ones and say what they
 *  are to them. Like paintSelection, this leaves the rows in place. */
function paintRelations() {
  const related = relatedToSelection();
  for (const row of $("rows").querySelectorAll<HTMLElement>(".row.mod")) {
    const kind = related.get(state.active[Number(row.dataset.index)]?.name ?? "");
    if (kind) row.dataset.rel = kind;
    else delete row.dataset.rel;
    const tag = row.querySelector<HTMLElement>(".badge.rel");
    if (!tag) continue;
    tag.hidden = !kind;
    tag.textContent = kind ? t(`rel.${kind}`) : "";
    tag.title = kind ? t(`rel.${kind}Title`) : "";
  }
}

/** A click on a row: alone it picks just that row (or lets go of it),
 *  with Ctrl it adds or removes the row, with Shift it picks the range
 *  from the row picked before. */
function selectRow(index: number, keys: { ctrlKey: boolean; metaKey: boolean; shiftKey: boolean }) {
  const item = state.active[index];
  if (!item) return;
  const picked = state.selection;
  const add = keys.ctrlKey || keys.metaKey;
  const from = state.anchor ? state.active.indexOf(state.anchor) : -1;
  if (keys.shiftKey && from >= 0) {
    if (!add) picked.clear();
    for (let i = Math.min(from, index); i <= Math.max(from, index); i++) picked.add(state.active[i]);
  } else {
    if (add) {
      if (!picked.delete(item)) picked.add(item);
    } else {
      const wasOnly = picked.size === 1 && picked.has(item);
      picked.clear();
      if (!wasOnly) picked.add(item);
    }
    state.anchor = item;
  }
  paintSelection();
}

function clearSelection() {
  if (state.selection.size === 0) return;
  state.selection.clear();
  paintSelection();
}

function renderCopyPaste() {
  const picked = state.selection.size;
  const copy = $<HTMLButtonElement>("copy-mods");
  copy.textContent = picked ? t("copy.selection", { n: picked }) : t("copy.whole");
  copy.disabled = state.active.length === 0;
  const held = state.clipboard?.game === state.game ? state.clipboard.mods.length : 0;
  const paste = $<HTMLButtonElement>("paste-mods");
  paste.textContent = held ? t("paste.button", { n: held }) : t("paste.empty");
  paste.disabled = held === 0;
}

/** Copy the picked mods - the whole list when none is picked - to paste
 *  them into another preset or profile of the same game. */
function copyMods() {
  const picked = state.active.filter((a) => state.selection.has(a));
  const items = picked.length ? picked : state.active;
  if (items.length === 0) return;
  state.clipboard = { game: state.game, mods: activeEntries(items) };
  renderCopyPaste();
  setStatus(t("copy.done", { n: items.length }), "ok");
}

/** Position of a load-order group, top first, for a row's token. */
function groupIndexOf(token: string): number {
  const { groups, category_groups } = state.constants;
  // the token of a pinned-to-top mod is the id of its group
  const at = groups.indexOf(category_groups[token] ?? token);
  return at >= 0 ? at : groups.indexOf("ui_other");
}

/** Add the copied mods that the list does not have yet, each at the top
 *  of its own group. Mods already in the list stay where they are. */
async function pasteMods() {
  const clip = state.clipboard;
  if (!clip || clip.game !== state.game) return;
  const have = activeNames();
  const wanted = clip.mods.filter((m) => !have.has(m.name));
  if (wanted.length === 0) {
    setStatus(t("paste.nothing", { n: clip.mods.length }));
    return;
  }
  const epoch = state.epoch;
  try {
    const resolved = await invoke<ActiveItem[]>("resolve_items", { game: state.game, items: wanted });
    if (epoch !== state.epoch) return;
    const now = activeNames();
    const items = resolved.filter((i) => !now.has(i.name));
    // bottom-most first, so mods of one group keep the order they had
    for (const item of [...items].reverse()) {
      const group = groupIndexOf(item.token);
      const at = state.active.findIndex((a) => groupIndexOf(a.token) >= group);
      state.active.splice(at < 0 ? state.active.length : at, 0, item);
    }
    // show what arrived
    state.selection = new Set(items);
    state.anchor = null;
    await relayout();
    const skipped = clip.mods.length - items.length;
    setStatus(
      t("paste.done", { n: items.length }) + (skipped ? t("paste.skipped", { n: skipped }) : "") + t("sort.saveNote"),
      "ok",
    );
  } catch (e) {
    setStatus(errorText(e), "error");
  }
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
  state.basePreset = null;
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
    // a press that never became a drag is a click: it selects
    if (!active) selectRow(from, ev);
    else if (row?.dataset.insertAt !== undefined) void moveActive(from, Number(row.dataset.insertAt));
  });

  rowsEl.addEventListener("pointercancel", finish);
  document.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape" && drag) finish();
  });

  // keyboard: Space picks the focused row, Ctrl+A all of them, Escape none
  rowsEl.addEventListener("keydown", (ev) => {
    if (ev.altKey || (ev.target as HTMLElement).closest("button")) return;
    const row = (ev.target as HTMLElement).closest<HTMLElement>(".row.mod");
    if (ev.key === " " && row?.dataset.index !== undefined) {
      ev.preventDefault();
      selectRow(Number(row.dataset.index), { ctrlKey: true, metaKey: false, shiftKey: ev.shiftKey });
    } else if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "a") {
      ev.preventDefault();
      state.selection = new Set(state.active);
      paintSelection();
    } else if (ev.key === "Escape" && !drag) {
      clearSelection();
    }
  });

  // keyboard: Ctrl+C / Ctrl+V copy and paste mods while the load order
  // (or nothing at all) has the focus; text fields keep their own
  document.addEventListener("keydown", (ev) => {
    if (!(ev.ctrlKey || ev.metaKey) || ev.altKey || ev.shiftKey) return;
    const key = ev.key.toLowerCase();
    if (key !== "c" && key !== "v") return;
    const focus = document.activeElement;
    const here = !focus || focus === document.body || !!focus.closest(".pane.order");
    if (!here || !$("modal").hidden || focus instanceof HTMLInputElement || focus instanceof HTMLTextAreaElement) return;
    if (key === "c" && window.getSelection()?.toString()) return; // text is being copied
    ev.preventDefault();
    if (key === "c") copyMods();
    else void pasteMods();
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
  const updateCheck = h("input", { type: "checkbox", id: "set-update", checked: s.update_check });
  const updateResult = h("span", { class: "form-note", id: "set-update-result", role: "status" });
  const checkNow: HTMLButtonElement = h(
    "button",
    {
      type: "button",
      class: "btn small",
      id: "set-check-update",
      onclick: async () => {
        checkNow.disabled = true;
        updateResult.textContent = t("settings.checking");
        try {
          const info = await checkUpdate(true);
          updateResult.textContent = info?.newer
            ? t("settings.updateFound", { version: info.latest })
            : t("settings.upToDate");
        } catch (e) {
          updateResult.textContent = errorText(e);
        }
        checkNow.disabled = false;
      },
    },
    t("settings.checkNow"),
  );
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
    h("label", { class: "form-check" }, updateCheck, h("span", {}, t("settings.updateCheck"))),
    h("div", { class: "form-note" }, t("settings.updateNote")),
    h(
      "div",
      { class: "form-inline" },
      h("span", {}, t("settings.version", { version: view.version })),
      checkNow,
      updateResult,
    ),
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
        update_check: updateCheck.checked,
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

// -------------------------------------------------------------- updates

/** Look up the latest release. The look at start (`manual` unset) does
 *  nothing when it is switched off in the settings. */
async function checkUpdate(manual: boolean): Promise<UpdateInfo | null> {
  const info = await invoke<UpdateInfo | null>("check_update", { manual });
  if (info) {
    state.update = info;
    renderUpdateButton();
  }
  return info;
}

/** The button in the top bar is only there while a newer version is out. */
function renderUpdateButton() {
  const button = $("open-update");
  const info = state.update;
  button.hidden = !info?.newer;
  if (info?.newer) button.textContent = t("update.button", { version: info.latest });
}

function openReleasePage(onError: (message: string) => void) {
  invoke("open_release_page").catch((e) => onError(errorText(e)));
}

async function openUpdate() {
  const info = state.update;
  if (!info?.newer) return;
  const error = h("div", { class: "form-error", role: "alert" });
  const showError = (message: string) => (error.textContent = message);

  const lines = [t("update.available", { latest: info.latest, current: info.current })];
  if (info.installable) {
    lines.push(t("update.how", { size: fmtSize(info.size) }));
    if (isDirty()) lines.push(t("update.dirty"));
  } else {
    lines.push(t("update.manual"));
  }
  const buttons: DialogButton<"later" | "page" | "install">[] = [{ label: t("update.later"), value: "later" }];
  if (info.installable) buttons.push({ label: t("update.install"), value: "install", kind: "primary" });
  else buttons.push({ label: t("update.page"), value: "page", kind: "primary" });

  const choice = await openDialog({
    title: t("update.title"),
    body: h(
      "div",
      { class: "form" },
      h("div", { class: "report" }, lines.join("\n\n")),
      // the notes open in the browser; the dialog stays for the decision
      info.installable &&
        h(
          "div",
          {},
          h(
            "button",
            { type: "button", class: "btn small", id: "update-notes", onclick: () => openReleasePage(showError) },
            t("update.notes"),
          ),
        ),
      error,
    ),
    dismiss: "later",
    buttons,
  });
  if (choice === "install") await installUpdate(info);
  else if (choice === "page") openReleasePage((message) => setStatus(message, "error"));
}

/** Download the new version, put it in place of the program file and
 *  start the app again. */
async function installUpdate(info: UpdateInfo) {
  const bar = h("progress", { class: "extract-bar", max: "1", value: "0" });
  const count = h("div", { class: "form-note" }, t("update.connecting"));
  const body = h("div", { class: "form" }, h("div", {}, t("update.downloading", { version: info.latest })), bar, count);
  const onProgress = new Channel<{ done: number; total: number }>();
  onProgress.onmessage = ({ done, total }) => {
    bar.max = Math.max(total, 1);
    bar.value = done;
    count.textContent = t("update.progress", { done: fmtSize(done), total: fmtSize(total) });
  };

  // closing the progress dialog in any way is a request to stop; it goes
  // away on its own when the download ends
  let running = true;
  let dialogOpen = true;
  void openDialog<void>({
    title: t("update.title"),
    body,
    dismiss: undefined,
    buttons: [{ label: t("common.cancel"), value: undefined }],
  }).then(() => {
    dialogOpen = false;
    if (running) invoke("cancel_update").catch(() => {});
  });

  let installed: string;
  try {
    installed = await invoke<string>("install_update", { onProgress });
  } catch (e) {
    running = false;
    if (dialogOpen) dismissDialog();
    const raw = e instanceof Error ? e.message : String(e);
    if (raw === "@update_cancelled") {
      setStatus(t("update.cancelled"));
      return;
    }
    const message = backendText(raw);
    setStatus(t("update.failed", { error: message }), "error");
    const toPage = await openDialog<boolean>({
      title: t("update.failedTitle"),
      body: h("div", { class: "report" }, t("update.failedBody", { error: message })),
      dismiss: false,
      buttons: [
        { label: t("common.close"), value: false },
        { label: t("update.page"), value: true, kind: "primary" },
      ],
    });
    if (toPage) openReleasePage((text) => setStatus(text, "error"));
    return;
  }
  running = false;
  if (dialogOpen) dismissDialog();
  setStatus(t("update.restarting", { version: installed }), "ok");
  try {
    await invoke("restart_app");
  } catch (e) {
    setStatus(errorText(e), "error");
  }
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
    state.basePreset = null;
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

// -------------------------------------------------------------- presets

/** Value of the option that opens the presets dialog; preset options carry
 *  their index, so no name can collide with it. */
const PRESET_MANAGE = "manage";
/** Value of the option that stores the edited list back into its preset. */
const PRESET_UPDATE = "update";

/** What makes two lists the same load order: the mods and their order. */
const listKey = (items: { name: string }[]) => items.map((i) => i.name).join("\n");

/** The preset the edited list is equal to, if any. */
function matchingPreset(): Preset | undefined {
  const key = listKey(state.active);
  return state.presets.find((p) => listKey(p.mods) === key);
}

/** The preset the list came from and has been edited away from, if any. */
function editedPreset(): Preset | undefined {
  if (matchingPreset()) return undefined;
  return state.presets.find((p) => p.name === state.basePreset);
}

/** The switcher above the load order: shows the preset the list equals
 *  (or was edited away from), and applies the one that is picked. */
function renderPresetSelect() {
  const select = $<HTMLSelectElement>("preset-select");
  const current = matchingPreset();
  const edited = editedPreset();
  select.replaceChildren(
    // only ever shown as the closed control's text, never offered
    h(
      "option",
      { value: "", hidden: true, selected: !current },
      edited ? t("preset.edited", { name: edited.name }) : t("preset.pick"),
    ),
    ...state.presets.map((p, i) =>
      h("option", { value: String(i), selected: p === current }, t("preset.option", { name: p.name, n: p.mods.length })),
    ),
    ...(edited ? [h("option", { value: PRESET_UPDATE }, t("preset.updateNamed", { name: edited.name }))] : []),
    h("option", { value: PRESET_MANAGE }, t("preset.manage")),
  );
  select.disabled = state.installs.length === 0;
}

/** Store the edited list back into the preset it came from. */
async function updateEditedPreset() {
  const preset = editedPreset();
  if (!preset) return;
  const game = state.game;
  const ok = await confirmModal(
    t("preset.updateTitle"),
    t("preset.updateBody", { name: preset.name, n: state.active.length, old: preset.mods.length }),
    t("preset.updateOk"),
  );
  if (!ok || game !== state.game) return;
  try {
    state.presets = await invoke<Preset[]>("save_preset", {
      game,
      name: preset.name,
      items: activeEntries(),
      overwrite: true,
    });
    renderPresetSelect();
    setStatus(
      t("preset.saved", { name: preset.name, n: state.active.length }) + (isDirty() ? t("preset.saveNote") : ""),
      "ok",
    );
  } catch (e) {
    setStatus(errorText(e), "error");
  }
}

/** Replace the edited list with the preset's. Nothing is written until
 *  Save, like every other change to the list. */
async function applyPreset(preset: Preset) {
  // edits that are neither saved nor kept in a preset would be gone
  if (isDirty() && !matchingPreset() && !(await confirmDiscard())) return;
  const epoch = state.epoch;
  try {
    const items = await invoke<ActiveItem[]>("resolve_items", { game: state.game, items: preset.mods });
    if (epoch !== state.epoch) return;
    state.active = items;
    state.basePreset = preset.name;
    await relayout();
    const missing = items.filter((i) => !i.installed).length;
    setStatus(
      t("preset.applied", { name: preset.name, n: items.length }) +
        (missing ? t("preset.appliedMissing", { n: missing }) : "") +
        (isDirty() ? t("preset.saveNote") : t("preset.sameAsProfile")),
      missing ? "info" : "ok",
    );
  } catch (e) {
    setStatus(errorText(e), "error");
  }
}

/** A button that asks for a second click before it acts. */
function armedButton(label: string, confirmLabel: string, action: () => void): HTMLButtonElement {
  const button = h("button", { class: "btn small" }, label);
  button.addEventListener("click", () => {
    if (button.dataset.armed === "1") return action();
    button.dataset.armed = "1";
    button.textContent = confirmLabel;
    button.classList.add("danger");
  });
  return button;
}

/** Store the current list as a preset, and update, rename, delete or apply
 *  the stored ones. */
async function openPresets() {
  const game = state.game;
  const list = h("div", { class: "preset-list" });
  const error = h("div", { class: "form-error", role: "alert" });
  const nameInput = h("input", {
    type: "text",
    maxlength: "60",
    placeholder: t("preset.namePlaceholder"),
    "aria-label": t("preset.name"),
  });
  let chosen: Preset | null = null;

  /** Run a command that changes the presets and returns the new list. */
  let busy = false;
  const change = async (command: string, args: Record<string, unknown>): Promise<boolean> => {
    // one at a time: a second Enter or click must not race the first
    if (busy) return false;
    busy = true;
    error.textContent = "";
    try {
      state.presets = await invoke<Preset[]>(command, { game, ...args });
      renderPresetSelect();
      await refresh();
      return true;
    } catch (e) {
      error.textContent = errorText(e);
      return false;
    } finally {
      busy = false;
    }
  };
  /** Enter that only ends an input method's composition (Hangul, pinyin)
   *  is not yet the Enter that confirms. */
  const confirms = (ev: KeyboardEvent) => ev.key === "Enter" && !ev.isComposing && ev.keyCode !== 229;

  const presetRow = (p: Preset, missing: number): HTMLElement => {
    const isCurrent = listKey(p.mods) === listKey(state.active);
    const name = h("span", { class: "preset-name", title: p.name }, p.name);
    const rename = () => {
      const input = h("input", { class: "preset-name", type: "text", maxlength: "60", "aria-label": t("preset.name") });
      input.value = p.name;
      let settled = false;
      const back = () => {
        settled = true;
        input.replaceWith(name);
      };
      const commit = async () => {
        if (settled) return;
        settled = true;
        const newName = input.value.trim();
        const wasBase = state.basePreset === p.name;
        if (!newName || newName === p.name || !(await change("rename_preset", { name: p.name, newName }))) {
          input.replaceWith(name);
        } else if (wasBase) {
          state.basePreset = newName;
          renderPresetSelect();
        }
      };
      input.addEventListener("keydown", (ev) => {
        if (confirms(ev)) void commit();
        else if (ev.key === "Escape") {
          // Escape leaves the name field, not the dialog
          ev.stopPropagation();
          back();
        }
      });
      input.addEventListener("blur", () => void commit());
      name.replaceWith(input);
      input.focus();
      input.select();
    };
    const update = armedButton(t("preset.update"), t("preset.reallyUpdate"), () => {
      void change("save_preset", { name: p.name, items: activeEntries(), overwrite: true }).then((ok) => {
        if (!ok) return;
        state.basePreset = p.name;
        setStatus(t("preset.saved", { name: p.name, n: state.active.length }), "ok");
      });
    });
    update.disabled = isCurrent;
    return h(
      "div",
      { class: "preset-row" },
      name,
      isCurrent ? h("span", { class: "badge cat", title: t("preset.currentTitle") }, t("preset.current")) : null,
      h(
        "span",
        { class: "muted" },
        t("preset.count", { n: p.mods.length }) + (missing ? t("preset.missing", { n: missing }) : ""),
      ),
      h(
        "button",
        {
          class: "btn small primary",
          disabled: isCurrent,
          onclick: () => {
            chosen = p;
            dismissDialog();
          },
        },
        t("common.apply"),
      ),
      update,
      h("button", { class: "btn small", onclick: rename }, t("preset.rename")),
      armedButton(t("common.delete"), t("backup.reallyDelete"), () => void change("delete_preset", { name: p.name })),
    );
  };

  const refresh = async () => {
    const presets = state.presets;
    // how much of each preset is not installed (any more)
    const missing = await Promise.all(
      presets.map((p) =>
        invoke<ActiveItem[]>("resolve_items", { game, items: p.mods })
          .then((items) => items.filter((i) => !i.installed).length)
          .catch(() => 0),
      ),
    );
    list.replaceChildren(
      ...(presets.length
        ? presets.map((p, i) => presetRow(p, missing[i]))
        : [h("div", { class: "empty" }, t("preset.empty"))]),
    );
  };

  const saveNew = async () => {
    const name = nameInput.value.trim();
    if (!name) {
      error.textContent = t("err.preset_empty_name");
      nameInput.focus();
      return;
    }
    if (await change("save_preset", { name, items: activeEntries(), overwrite: false })) {
      nameInput.value = "";
      state.basePreset = name;
      setStatus(t("preset.saved", { name, n: state.active.length }), "ok");
    }
  };
  nameInput.addEventListener("keydown", (ev) => {
    if (confirms(ev)) void saveNew();
  });

  await refresh();
  const opened = openDialog<void>({
    title: t("preset.title", { game: game.toUpperCase() }),
    body: h(
      "div",
      { class: "form" },
      h("div", {}, t("preset.intro")),
      h("div", { class: "form-note" }, t("preset.note")),
      h(
        "div",
        { class: "preset-new" },
        nameInput,
        h("button", { class: "btn primary", onclick: () => void saveNew() }, t("preset.saveCurrent", { n: state.active.length })),
      ),
      list,
      error,
    ),
    wide: true,
    dismiss: undefined,
    buttons: [{ label: t("common.close"), value: undefined }],
  });
  // with nothing stored yet, the name is the next thing to type
  if (state.presets.length === 0) nameInput.focus();
  await opened;
  const picked = chosen as Preset | null;
  if (picked) await applyPreset(picked);
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
  __scsmm: { exportShareTo, importShareFrom, exportMapComboTo, importMapComboFrom, extractModTo, openPresets },
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
  $("open-update").addEventListener("click", () => void openUpdate());
  $("open-settings").addEventListener("click", () => void openSettings());
  $("open-backups").addEventListener("click", () => void openBackups());
  $("open-share").addEventListener("click", () => void openShareMenu());
  $("copy-mods").addEventListener("click", () => copyMods());
  $("paste-mods").addEventListener("click", () => void pasteMods());
  $<HTMLSelectElement>("preset-select").addEventListener("change", (ev) => {
    const value = (ev.target as HTMLSelectElement).value;
    // show what the list is until the choice has actually been applied
    renderPresetSelect();
    if (value === PRESET_MANAGE) void openPresets();
    else if (value === PRESET_UPDATE) void updateEditedPreset();
    else if (state.presets[Number(value)]) void applyPreset(state.presets[Number(value)]);
  });

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
  renderUpdateButton();
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
  $("app-version").textContent = state.settings.version;
  applyLanguage();
  // in the background, and quietly: being offline is no error at start
  checkUpdate(false).catch(() => {});
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
