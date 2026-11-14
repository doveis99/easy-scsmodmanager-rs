import { invoke } from "@tauri-apps/api/core";

// ---------------------------------------------------------------- types

type GameId = "ets2" | "ats";
type Compat = "compatible" | "incompatible" | "unspecified" | "unknown_game_version";

interface InstallInfo {
  game: GameId;
  short_name: string;
  documents_dir: string;
  workshop_dir: string | null;
  version: string | null;
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

interface WorkshopFetch {
  asked: number;
  titles: number;
  previews: number;
  error: string | null;
}

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
  filter: { q: "", category: "", source: "", sort: "name" },
  /** Bumped per load so a slow response for an old game/profile is dropped. */
  epoch: 0,
};

const icons = new Map<string, string | null>();

// ------------------------------------------------------------ dom utils

function $<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el as T;
}

type Child = Node | string | null | undefined | false;

/** Build an element. Text always goes through textContent: mod names and
 *  descriptions come from untrusted archives. */
function h<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  props: Partial<Record<string, string | boolean | ((ev: Event) => void)>> = {},
  ...children: Child[]
): HTMLElementTagNameMap[K] {
  const el = document.createElement(tag);
  for (const [key, value] of Object.entries(props)) {
    if (typeof value === "function") {
      el.addEventListener(key.replace(/^on/, ""), value as EventListener);
    } else if (typeof value === "boolean") {
      if (value) el.setAttribute(key, "");
    } else if (value !== undefined) {
      el.setAttribute(key, value);
    }
  }
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    el.append(child);
  }
  return el;
}

function setStatus(message: string, kind: "info" | "error" | "ok" = "info") {
  const el = $("status");
  el.textContent = message;
  el.dataset.kind = kind;
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function confirmModal(title: string, body: string, okLabel: string): Promise<boolean> {
  return openModal(title, body, okLabel, false);
}

/** A read-only report: preformatted, selectable text and a single button. */
function infoModal(title: string, body: string): Promise<boolean> {
  return openModal(title, body, "닫기", true);
}

function openModal(title: string, body: string, okLabel: string, info: boolean): Promise<boolean> {
  return new Promise((resolve) => {
    const modal = $("modal");
    $("modal-title").textContent = title;
    const bodyEl = $("modal-body");
    bodyEl.textContent = body;
    bodyEl.classList.toggle("pre", info);
    modal.classList.toggle("wide", info);
    const ok = $<HTMLButtonElement>("modal-ok");
    const cancel = $<HTMLButtonElement>("modal-cancel");
    cancel.hidden = info;
    ok.textContent = okLabel;
    const close = (answer: boolean) => {
      modal.hidden = true;
      ok.onclick = cancel.onclick = null;
      document.removeEventListener("keydown", onKey);
      resolve(answer);
    };
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Escape") close(false);
    };
    ok.onclick = () => close(true);
    cancel.onclick = () => close(false);
    document.addEventListener("keydown", onKey);
    modal.hidden = false;
    ok.focus();
  });
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
    state.profilePath = best?.path ?? "";
    renderProfileSelect();
    await loadProfile();
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
  const [layout, conflicts] = await Promise.all([
    invoke<Layout>("layout_rows", { tokens: state.active.map((a) => a.token) }),
    invoke<ConflictReport>("analyze_conflicts", { game: state.game, items: activeEntries() }),
  ]);
  if (epoch !== state.epoch) return;
  state.layout = layout;
  state.conflicts = conflicts;
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
  const { q, category, source, sort } = state.filter;
  const needle = q.trim().toLowerCase();
  const out = state.mods.filter((m) => {
    if (category && !m.categories.includes(category)) return false;
    if (source && m.source !== source) return false;
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
      h("div", { class: "card-title" }, m.display_name),
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
      categorySelect,
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
  const { rows, misplaced } = state.layout;
  const dirty = isDirty();

  const overridden = Object.keys(state.conflicts.overrides).length;
  const incompatible = state.active.filter((a) => a.compat === "incompatible").length;
  $("order-summary").textContent =
    `활성 ${state.active.length}개` +
    (misplaced ? ` · 그룹에 맞지 않는 모드 ${misplaced}개` : "") +
    (overridden ? ` · 덮어써지는 모드 ${overridden}개` : "") +
    (incompatible ? ` · 버전 불일치 ${incompatible}개` : "");
  $("show-conflicts").hidden =
    overridden === 0 && Object.keys(state.conflicts.frequent).length === 0;
  $<HTMLButtonElement>("auto-sort").disabled = misplaced === 0;
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
      class: "row mod" + (row.misplaced ? " misplaced" : "") + (item.installed ? "" : " missing"),
      tabindex: "0",
      "data-index": String(row.index),
    },
    h("span", { class: "grip", "aria-hidden": "true" }, "⋮⋮"),
    h("span", { class: "row-name" }, item.display_name || item.name),
    row.misplaced
      ? h("span", { class: "badge warn" }, `→ ${groupLabel(row.expected_group_id)}`)
      : null,
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

async function autoSort() {
  const order = await invoke<number[]>("auto_sort", {
    tokens: state.active.map((a) => a.token),
  });
  state.active = order.map((i) => state.active[i]);
  await relayout();
  setStatus("모든 모드를 각자의 그룹으로 정렬했습니다. 그룹 안의 순서는 그대로입니다. 저장해야 게임에 반영됩니다.");
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
    const backup = await invoke<string>("save_load_order", {
      game: state.game,
      profilePath: state.profilePath,
      items: activeEntries(),
    });
    state.saved = JSON.stringify(state.active);
    const profile = state.profiles.find((p) => p.path === state.profilePath);
    if (profile) profile.active_count = state.active.length;
    renderProfileSelect();
    renderOrder();
    setStatus(`저장했습니다. 백업: ${backup}`, "ok");
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

  $<HTMLSelectElement>("profile-select").addEventListener("change", async (ev) => {
    const select = ev.target as HTMLSelectElement;
    if (isDirty() && !(await confirmDiscard())) {
      select.value = state.profilePath;
      return;
    }
    state.profilePath = select.value;
    try {
      await loadProfile();
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
    [state.constants, state.installs] = await Promise.all([
      invoke<Constants>("constants"),
      invoke<InstallInfo[]>("get_installs"),
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
    setStatus("ETS2 / ATS 데이터 폴더를 찾지 못했습니다.", "error");
    return;
  }
  const first = state.installs.find((i) => i.game === "ets2") ?? state.installs[0];
  await loadGame(first.game);
}

void boot();
