import { invoke } from "@tauri-apps/api/core";
import { open as pickOpen, save as pickSave } from "@tauri-apps/plugin-dialog";
import { $, confirmModal, dismissDialog, h, infoModal, openDialog } from "./dom";

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
  category_override: string | null;
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

interface Settings {
  workshop_lookup: boolean;
  backup_keep: number;
  map_base_names: string[];
  last_game: GameId | null;
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
}

// --------------------------------------------------------------- labels

const CATEGORY_LABEL: Record<string, string> = {
  truck: "트럭",
  trailer: "트레일러",
  interior: "인테리어",
  tuning_parts: "튜닝 파츠",
  ai_traffic: "AI 교통",
  sound: "사운드",
  paint_job: "도색",
  cargo_pack: "화물",
  map: "맵",
  ui: "UI",
  weather_setup: "날씨",
  physics: "물리",
  graphics: "그래픽",
  models: "모델",
  movers: "무버",
  walkers: "보행자",
  prefabs: "프리팹",
  other: "기타",
};

const GROUP_LABEL: Record<string, string> = {
  map_base: "맵 기반 · 배경 · 로딩 화면",
  graphics_weather: "그래픽 · 날씨",
  sound: "사운드",
  physics: "물리",
  ui_other: "UI · 기타",
  tuning_interior: "튜닝 · 인테리어",
  ai_traffic: "AI 교통",
  cargo: "화물",
  paint_jobs: "도색",
  trailers: "트레일러",
  trucks: "트럭",
  maps: "맵",
};

const catLabel = (token: string) => CATEGORY_LABEL[token] ?? token;
const groupLabel = (id: string) => GROUP_LABEL[id] ?? id;

// ---------------------------------------------------------------- state

const state = {
  constants: { categories: [], groups: [] } as Constants,
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

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
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
  setStatus("모드를 스캔하는 중…");
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
      `모드 ${mods.length}개를 ${ms}ms에 스캔했습니다.` +
        (unreadable ? ` 읽지 못한 모드 ${unreadable}개.` : ""),
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
    setStatus(`창작마당에서 이름 ${r.titles}개, 미리보기 ${r.previews}개를 받아 왔습니다.`, "ok");
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
    if (rest > 0) lines.push(`  … 외 ${rest}개`);
  };
  const override = state.conflicts.overrides[name];
  if (override) {
    lines.push(
      override.severity === "full"
        ? `겹치는 def 파일 ${override.shared_total}개가 모두 위쪽 모드에 덮어써집니다.`
        : `겹치는 def 파일 ${override.shared_total}개 중 ${override.lost.length}개가 위쪽 모드에 덮어써집니다.`,
    );
    const shown = limit === undefined ? override.lost : override.lost.slice(0, limit);
    for (const l of shown) lines.push(`  ${l.file} → ${displayNameOf(l.winner)}`);
    more(override.lost.length - shown.length);
  }
  const frequent = state.conflicts.frequent[name];
  if (frequent) {
    lines.push(
      override
        ? "여러 모드가 흔히 함께 수정하는 파일도 있습니다 (충돌 아님)."
        : "여러 모드가 흔히 함께 수정하는 파일입니다 (충돌 아님).",
    );
    const shown = limit === undefined ? frequent : frequent.slice(0, limit);
    for (const f of shown) lines.push(`  ${f.file} (모드 ${f.owners}개)`);
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
    `게임: ${state.game.toUpperCase()}${version ? ` ${version}` : ""}   프로필: ${profile}`,
    `덮어써지는 모드 ${Object.keys(state.conflicts.overrides).length}개`,
    "아래쪽 모드의 파일이 위쪽 모드에 가려진다는 안내이며 오류가 아닙니다. 맵 모드에서는 의도된 경우가 많습니다.",
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
  const label = { full: "전부 덮어써짐", partial: "일부 덮어써짐", frequent: "흔히 공유되는 파일" }[kind];
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
  return `이 모드가 선언한 호환 버전: ${versions.join(", ")}` + (game ? `\n현재 게임 버전: ${game}` : "");
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
          title: install ? install.documents_dir : "설치를 찾지 못했습니다",
          onclick: () => void switchGame(g),
        },
        g.toUpperCase(),
      );
    }),
  );
  const current = state.installs.find((i) => i.game === state.game);
  $("game-version").textContent = current?.version ? `게임 버전 ${current.version}` : "";
}

async function switchGame(game: GameId) {
  if (game === state.game) return;
  if (isDirty() && !(await confirmDiscard())) return;
  await loadGame(game);
}

function confirmDiscard(): Promise<boolean> {
  return confirmModal(
    "저장하지 않은 변경 사항",
    "로드 순서를 바꿨지만 아직 저장하지 않았습니다. 변경 사항을 버릴까요?",
    "버리기",
  );
}

function renderProfileSelect() {
  const select = $<HTMLSelectElement>("profile-select");
  select.replaceChildren(
    ...state.profiles.map((p) =>
      h(
        "option",
        { value: p.path, disabled: !!p.error, selected: p.path === state.profilePath },
        p.error ? `${p.profile_name} (읽기 실패)` : `${p.profile_name} · 활성 ${p.active_count}개`,
      ),
    ),
  );
  if (state.profiles.length === 0) {
    select.append(h("option", { value: "" }, "프로필 없음"));
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
    out.sort((a, b) => a.display_name.localeCompare(b.display_name, "ko"));
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
      ? `모드 ${state.mods.length}개`
      : `모드 ${state.mods.length}개 중 ${mods.length}개 표시`;

  const grid = $("grid");
  if (mods.length === 0) {
    grid.replaceChildren(
      h("div", { class: "empty" }, state.mods.length ? "조건에 맞는 모드가 없습니다." : "모드가 없습니다."),
    );
    return;
  }
  grid.replaceChildren(...mods.map((m) => modCard(m, active.has(m.id))));
}

function modCard(m: Mod, isActive: boolean): HTMLElement {
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
      title: "분류 변경",
      "aria-label": "분류 변경",
      onchange: (ev: Event) => {
        const value = (ev.target as HTMLSelectElement).value;
        void setOverride(m, value || null);
      },
    },
    h("option", { value: "", selected: !m.category_override }, "자동 분류"),
    ...state.constants.categories.map((c) =>
      h("option", { value: c, selected: m.category_override === c }, catLabel(c)),
    ),
  );

  const meta = [m.author, m.version && `v${m.version}`].filter(Boolean).join(" · ");
  return h(
    "article",
    { class: "card" + (isActive ? " active" : "") + (m.error ? " broken" : ""), title: m.description ?? m.path },
    iconHolder,
    h(
      "div",
      { class: "card-body" },
      h("button", { class: "card-title", title: "상세 보기", onclick: () => void openDetails(m) }, m.display_name),
      h("div", { class: "card-meta muted" }, meta || m.file_name),
      h(
        "div",
        { class: "badges" },
        ...m.categories.map((c) =>
          h("span", { class: "badge cat" + (m.category_override ? " pinned" : "") }, catLabel(c)),
        ),
        h("span", { class: "badge src" }, m.source === "workshop" ? "창작마당" : "로컬"),
        m.compat === "incompatible"
          ? h("span", { class: "badge err", title: compatTitle(m.compatible_versions) }, "버전 불일치")
          : null,
        m.error ? h("span", { class: "badge err", title: m.error }, "읽기 실패") : null,
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
            title: m.favorite ? "즐겨찾기 해제" : "즐겨찾기",
            "aria-label": m.favorite ? "즐겨찾기 해제" : "즐겨찾기",
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
        isActive ? "비활성화" : "활성화",
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
    `활성 ${state.active.length}개` +
    (misplaced ? ` · 그룹에 맞지 않는 모드 ${misplaced}개` : "") +
    (heldCount ? ` · 순서 유지 ${heldCount}개` : "") +
    (overridden ? ` · 덮어써지는 모드 ${overridden}개` : "") +
    (incompatible ? ` · 버전 불일치 ${incompatible}개` : "");
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
    item.installed ? null : h("span", { class: "badge err" }, "미설치"),
    item.compat === "incompatible"
      ? h(
          "span",
          {
            class: "badge err",
            title: compatTitle(state.mods.find((m) => m.id === item.name)?.compatible_versions ?? []),
          },
          "버전 불일치",
        )
      : null,
    conflictGlyph(item),
    h(
      "button",
      { class: "icon-btn", title: "비활성화", "aria-label": "비활성화", onclick: () => void removeActive(item.name) },
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
    return h("span", { class: "badge warn", title: `분류상 '${group}' 그룹에 속합니다.` }, `→ ${group}`);
  }
  const partners = (state.plan.partners[item.name] ?? []).map(displayNameOf);
  const shown = partners.slice(0, 6).join(", ") + (partners.length > 6 ? ` 외 ${partners.length - 6}개` : "");
  return h(
    "span",
    {
      class: "badge keep",
      title:
        `분류상 '${group}' 그룹이지만 이 위치가 맞습니다.\n` +
        `같은 파일을 가진 모드와의 위아래 순서를 지켜야 해서 옮기지 않습니다: ${shown}`,
    },
    "순서 유지",
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
    "모드를 그룹별로 정렬했습니다. 같은 파일을 가진 모드끼리는 위아래 순서를 그대로 두어 우선순위가 바뀌지 않습니다." +
      (held.length ? ` 그 때문에 ${held.length}개는 그룹 밖에 그대로 있습니다.` : "") +
      " 저장해야 게임에 반영됩니다.",
  );
}

async function revert() {
  state.active = JSON.parse(state.saved) as ActiveItem[];
  await relayout();
  setStatus("변경 사항을 되돌렸습니다.");
}

async function save() {
  if (!state.profilePath || !isDirty()) return;
  const game = state.game.toUpperCase();
  const ok = await confirmModal(
    "로드 순서 저장",
    `프로필의 활성 모드 목록을 덮어씁니다. 저장 전에 프로필 폴더 전체를 자동으로 백업합니다. ${game}가 실행 중이면 먼저 종료하세요. 게임이 종료할 때 프로필을 다시 써서 변경 사항이 사라질 수 있습니다.`,
    "저장",
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
      `저장했습니다. 백업: ${result.backup}` + (result.pruned ? ` (오래된 백업 ${result.pruned}개 정리)` : ""),
      "ok",
    );
  } catch (e) {
    setStatus(`저장 실패: ${errorText(e)}`, "error");
  }
}

async function setOverride(m: Mod, token: string | null) {
  try {
    await invoke("set_category_override", { game: state.game, id: m.id, token });
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
      token
        ? `'${m.display_name}'의 분류를 ${catLabel(token)}(으)로 고정했습니다.`
        : `'${m.display_name}'의 분류를 자동으로 되돌렸습니다.`,
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

type DetailAction = "close" | "reveal" | "workshop" | "delete";

async function openDetails(m: Mod) {
  const icon = icons.get(`${state.game}/${m.id}`);
  const rows: [string, string][] = [
    ["제작자", m.author || "-"],
    ["버전", m.version || "-"],
    ["분류", m.categories.map(catLabel).join(", ") + (m.category_override ? " (수동 고정)" : "")],
    ["모드가 선언한 분류", m.manifest_categories.join(", ") || "-"],
    ["출처", m.source === "workshop" ? "Steam 창작마당" : "로컬 모드 폴더"],
    ["형식", m.format],
    ["호환 버전", m.compatible_versions.join(", ") || "선언 없음"],
    ["설치일", m.installed_at ? new Date(m.installed_at * 1000).toLocaleString("ko-KR") : "-"],
    ["파일", m.path],
  ];
  if (m.error) rows.push(["읽기 오류", m.error]);
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
    { label: "닫기", value: "close" },
    { label: "파일 위치 열기", value: "reveal" },
  ];
  if (m.source === "workshop") buttons.push({ label: "창작마당 페이지", value: "workshop" });
  else buttons.push({ label: "휴지통으로 삭제", value: "delete", kind: "danger" });

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
    "모드 삭제",
    `'${m.display_name}' 파일을 휴지통으로 옮깁니다. 휴지통에서 복원할 수 있습니다.` +
      (users.length
        ? `\n\n이 모드는 다음 프로필의 로드 순서에 들어 있습니다: ${users.join(", ")}\n삭제해도 로드 순서에서는 빠지지 않고 '미설치'로 표시됩니다.`
        : ""),
    "휴지통으로 이동",
    true,
  );
  if (!ok) return;
  try {
    const r = await invoke<DeleteResult>("delete_mods", { game, ids: [m.id] });
    if (r.deleted.length === 0) {
      setStatus(`삭제하지 못했습니다: ${r.failed[0]?.error ?? "알 수 없는 오류"}`, "error");
      return;
    }
    state.mods = state.mods.filter((x) => !r.deleted.includes(x.id));
    await resolveLists();
    setStatus(`'${m.display_name}'을(를) 휴지통으로 옮겼습니다.`, "ok");
  } catch (e) {
    setStatus(errorText(e), "error");
  }
}

async function openBackups() {
  const game = state.game;
  const profilePath = state.profilePath;
  if (!profilePath) {
    setStatus("선택된 프로필이 없습니다.", "error");
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
            const del = h("button", { class: "btn small" }, "삭제");
            // deleting a backup cannot be undone: ask for a second click
            del.addEventListener("click", async () => {
              if (del.dataset.armed !== "1") {
                del.dataset.armed = "1";
                del.textContent = "정말 삭제";
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
                "복원",
              ),
              del,
            );
          })
        : [h("div", { class: "empty" }, "아직 백업이 없습니다. 로드 순서를 저장하면 자동으로 만들어집니다.")]),
    );
    const total = backups.reduce((sum, b) => sum + b.size_bytes, 0);
    const keep = state.settings?.settings.backup_keep ?? 0;
    footer.textContent =
      `백업 ${backups.length}개 · 합계 ${fmtSize(total)} · ` +
      (keep ? `프로필당 최근 ${keep}개만 보관` : "개수 제한 없이 보관") +
      (state.settings ? `\n위치: ${state.settings.backup_root}` : "");
  };

  try {
    await refresh();
  } catch (e) {
    setStatus(errorText(e), "error");
    return;
  }
  await openDialog<void>({
    title: `프로필 백업 — ${profileName}`,
    body: h("div", { class: "form" }, list, footer, error),
    wide: true,
    dismiss: undefined,
    buttons: [{ label: "닫기", value: undefined, kind: "primary" }],
  });
  const chosen = restore as BackupDto | null;
  if (!chosen) return;

  const ok = await confirmModal(
    "백업 복원",
    `${fmtStamp(chosen.timestamp)} 백업으로 프로필 '${profileName}'을(를) 되돌립니다.\n\n` +
      "복원 전에 현재 상태를 먼저 백업하므로 다시 되돌릴 수 있습니다. 세이브 파일도 그 시점으로 돌아갑니다." +
      (isDirty() ? "\n저장하지 않은 로드 순서 변경은 사라집니다." : "") +
      `\n\n${game.toUpperCase()}가 실행 중이면 먼저 종료하세요.`,
    "복원",
    true,
  );
  if (!ok) return;
  try {
    const safety = await invoke<string>("restore_backup", { game, profilePath, backupPath: chosen.path });
    state.profiles = await invoke<Profile[]>("list_profiles", { game });
    renderProfileSelect();
    await loadProfile();
    setStatus(`복원했습니다. 복원 직전 상태의 백업: ${safety}`, "ok");
  } catch (e) {
    setStatus(`복원 실패: ${errorText(e)}`, "error");
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
      placeholder: detected ? `자동 감지: ${detected}` : "자동 감지 실패 — 경로를 직접 입력하세요",
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
    field("문서 폴더", group.documents, "mod, profiles 폴더가 들어 있는 곳입니다. 비워 두면 자동 감지합니다."),
    field("창작마당 폴더", group.workshop, "steamapps/workshop/content/<앱 ID> 폴더입니다. 비워 두면 자동 감지합니다."),
  ];
  const body = h(
    "div",
    { class: "form" },
    h(
      "label",
      { class: "form-check" },
      lookup,
      h("span", {}, "창작마당 모드의 이름과 미리보기를 Steam에서 가져오기"),
    ),
    h("div", { class: "form-note" }, "스캔 후 캐시에 없는 창작마당 ID만 Steam 공개 API로 보냅니다."),
    field("프로필당 백업 보관 개수", keep, `0이면 모두 보관합니다. 백업 위치: ${view.backup_root}`),
    ...pathFields("ETS2 경로", inputs.ets2),
    ...pathFields("ATS 경로", inputs.ats),
    h("h3", {}, "로드 순서 맨 위에 두는 모드"),
    field(
      "이름 조각 (한 줄에 하나)",
      names,
      "모드 이름에 이 글자가 들어 있으면 '맵 기반 · 배경 · 로딩 화면' 그룹으로 분류합니다.",
    ),
    error,
  );

  const text = (el: HTMLInputElement) => el.value.trim() || null;
  const saved = await openDialog<boolean>({
    title: "설정",
    body,
    wide: true,
    dismiss: false,
    buttons: [
      { label: "취소", value: false },
      { label: "저장", value: true, kind: "primary" },
    ],
    beforeClose: async (value) => {
      if (!value) return;
      const count = Number(keep.value);
      if (!Number.isInteger(count) || count < 0 || count > 1000) {
        error.textContent = "백업 보관 개수는 0에서 1000 사이의 정수여야 합니다.";
        return false;
      }
      const next: Settings = {
        ...s,
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
  if (isDirty()) {
    renderGameSwitch();
    setStatus("설정을 저장했습니다. 저장하지 않은 로드 순서가 있어 다시 스캔하지 않았습니다. '다시 스캔'을 누르면 반영됩니다.", "ok");
    return;
  }
  const target = state.installs.find((i) => i.game === state.game) ?? state.installs[0];
  if (!target) {
    renderGameSwitch();
    setStatus("설정을 저장했습니다. ETS2 / ATS 데이터 폴더를 찾지 못했습니다.", "error");
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
    setStatus(`로드 순서 ${count}개를 내보냈습니다: ${path}`, "ok");
  } catch (e) {
    setStatus(`내보내기 실패: ${errorText(e)}`, "error");
  }
}

/** Preview a share file (or a profile.sii) and, on the user's go-ahead,
 *  replace the edited list with it. Nothing is written until Save. */
async function importShareFrom(path: string) {
  let preview: SharePreview;
  try {
    preview = await invoke<SharePreview>("preview_share", { game: state.game, path });
  } catch (e) {
    setStatus(`가져오기 실패: ${errorText(e)}`, "error");
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
        ? h("span", { class: "badge warn", title: `설치된 버전 ${e.local_version} · 공유 목록 ${e.share_version}` }, "구버전")
        : null,
      e.status === "missing_local" ? h("span", { class: "badge err" }, "미설치 · 로컬") : null,
      e.status === "missing_workshop"
        ? h(
            "button",
            {
              class: "btn small",
              title: "Steam 창작마당 페이지 열기",
              onclick: () => {
                const id = e.workshop_url?.split("id=")[1] ?? "";
                invoke("open_workshop_item", { workshopId: id }).catch((err) => setStatus(errorText(err), "error"));
              },
            },
            "미설치 · 창작마당 열기",
          )
        : null,
    ),
  );
  const source =
    preview.kind === "profile"
      ? `프로필 파일${preview.profile_name ? ` '${preview.profile_name}'` : ""}`
      : `공유 파일${preview.profile_name ? ` (프로필 '${preview.profile_name}')` : ""}`;
  const summary =
    `${source}: 모드 ${preview.entries.length}개 · 설치됨 ${preview.entries.length - missing.length}개` +
    (missing.length ? ` · 미설치 ${missing.length}개` : "") +
    (outdated.length ? ` · 구버전 ${outdated.length}개` : "");
  const body = h(
    "div",
    { class: "form" },
    h("div", {}, summary),
    h(
      "div",
      { class: "form-note" },
      "가져오면 지금 편집 중인 로드 순서가 이 목록으로 바뀝니다. 프로필 파일은 '저장'을 눌러야 바뀝니다.",
    ),
    h("div", { class: "share-list" }, ...rows),
  );
  type Choice = "cancel" | "installed" | "all";
  const buttons: { label: string; value: Choice; kind?: "primary" }[] = [{ label: "취소", value: "cancel" }];
  if (missing.length) {
    buttons.push({ label: "설치된 모드만 가져오기", value: "installed", kind: "primary" });
    buttons.push({ label: "미설치 포함 전부", value: "all" });
  } else {
    buttons.push({ label: "가져오기", value: "all", kind: "primary" });
  }
  const choice = await openDialog<Choice>({ title: "로드 순서 가져오기", body, wide: true, dismiss: "cancel", buttons });
  if (choice === "cancel") return;

  const picked = preview.entries.filter((e) => choice === "all" || e.status === "found");
  try {
    state.active = await invoke<ActiveItem[]>("resolve_items", {
      game: state.game,
      items: picked.map(({ name, display_name }) => ({ name, display_name })),
    });
    await relayout();
    setStatus(`모드 ${picked.length}개를 가져왔습니다. '저장'을 눌러야 게임에 반영됩니다.`, "ok");
  } catch (e) {
    setStatus(`가져오기 실패: ${errorText(e)}`, "error");
  }
}

async function exportMapComboTo(path: string) {
  try {
    const count = await invoke<number>("export_map_combo", { game: state.game, items: activeEntries(), path });
    setStatus(`맵 ${count}개의 순서를 내보냈습니다: ${path}`, "ok");
  } catch (e) {
    setStatus(`내보내기 실패: ${errorText(e)}`, "error");
  }
}

async function importMapComboFrom(path: string) {
  let preview: MapComboPreview;
  try {
    preview = await invoke<MapComboPreview>("preview_map_combo", { game: state.game, items: activeEntries(), path });
  } catch (e) {
    setStatus(`가져오기 실패: ${errorText(e)}`, "error");
    return;
  }
  if (preview.missing.length) {
    await infoModal(
      "맵 콤보를 적용할 수 없습니다",
      "이 콤보에 있는 다음 맵이 현재 로드 순서의 맵 그룹에 없습니다. 먼저 설치하고 활성화한 뒤 다시 가져오세요.\n\n" +
        preview.missing.map((n) => `- ${n}`).join("\n"),
    );
    return;
  }
  const byName = new Map(state.active.map((a) => [a.name, a]));
  state.active = preview.order.flatMap((n) => byName.get(n) ?? []);
  await relayout();
  setStatus(`맵 ${preview.count}개를 콤보 순서로 정렬했습니다. '저장'을 눌러야 게임에 반영됩니다.`, "ok");
  if (preview.outdated.length) {
    await infoModal(
      "콤보보다 오래된 맵",
      "순서는 적용했습니다. 다음 맵은 콤보를 만든 사람보다 낮은 버전이 설치돼 있습니다.\n\n" +
        preview.outdated.map((o) => `- ${o.name}: 설치됨 ${o.local}, 콤보 ${o.combo}`).join("\n"),
    );
  }
}

type ShareAction = "close" | "export" | "import" | "combo-export" | "combo-import";

async function openShareMenu() {
  const body = h(
    "div",
    { class: "form" },
    h("div", {}, "로드 순서를 파일로 주고받습니다. 공유 파일에는 모드 이름과 버전만 들어 있고, 모드 파일 자체는 들어 있지 않습니다."),
    h(
      "div",
      { class: "form-note" },
      "전체 가져오기는 공유 파일(.modshare.json)과 다른 사람의 profile.sii를 모두 읽습니다. 맵 콤보는 맵 그룹의 순서만 다룹니다.",
    ),
  );
  const action = await openDialog<ShareAction>({
    title: "공유",
    body,
    wide: true,
    dismiss: "close",
    buttons: [
      { label: "닫기", value: "close" },
      { label: "전체 내보내기", value: "export", kind: "primary" },
      { label: "전체 가져오기", value: "import" },
      { label: "맵 콤보 내보내기", value: "combo-export" },
      { label: "맵 콤보 가져오기", value: "combo-import" },
    ],
  });
  try {
    if (action === "export") {
      const path = await pickSave({ defaultPath: `${fileStem(currentProfileName())}.modshare.json`, filters: JSON_FILTER });
      if (path) await exportShareTo(path);
    } else if (action === "import") {
      const path = await pickOpen({
        multiple: false,
        filters: [{ name: "공유 파일 또는 프로필", extensions: ["json", "sii"] }],
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
  __scsmm: { exportShareTo, importShareFrom, exportMapComboTo, importMapComboFrom },
});

// ----------------------------------------------------------------- boot

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
    });
  bindSelect("filter-category", "category");
  bindSelect("filter-source", "source");
  bindSelect("sort", "sort");
  $<HTMLInputElement>("filter-favorites").addEventListener("change", (ev) => {
    filter.favorites = (ev.target as HTMLInputElement).checked;
    renderLibrary();
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
  $("show-conflicts").addEventListener("click", () => void infoModal("def 파일 충돌", conflictReport()));
  $("revert").addEventListener("click", () => void revert());
  $("save").addEventListener("click", () => void save());
}

async function boot() {
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
  $("filter-category").replaceChildren(
    h("option", { value: "" }, "모든 분류"),
    ...state.constants.categories.map((c) => h("option", { value: c }, catLabel(c))),
  );
  if (state.installs.length === 0) {
    renderGameSwitch();
    renderAll();
    setStatus("ETS2 / ATS 데이터 폴더를 찾지 못했습니다. 설정에서 경로를 직접 지정할 수 있습니다.", "error");
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
