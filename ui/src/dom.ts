// Small DOM helpers and the one modal dialog the app uses.

import { t } from "./i18n";

export function $<T extends HTMLElement>(id: string): T {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el as T;
}

export type Child = Node | string | null | undefined | false;

/** Build an element. Text always goes through textContent: mod names and
 *  descriptions come from untrusted archives. */
export function h<K extends keyof HTMLElementTagNameMap>(
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

export interface DialogButton<T> {
  label: string;
  value: T;
  kind?: "primary" | "danger";
}

export interface DialogOptions<T> {
  title: string;
  /** Plain text, or a node the caller owns (and may keep updating). */
  body: Node | string;
  buttons: DialogButton<T>[];
  /** Value returned for Escape or a click on the backdrop. */
  dismiss: T;
  wide?: boolean;
  /** Preformatted, selectable text (reports). */
  pre?: boolean;
  /** Runs when a button is pressed; resolve to `false` to keep the dialog
   *  open (a failed save, or an action that should not close it). */
  beforeClose?: (value: T) => boolean | void | Promise<boolean | void>;
}

let closeCurrent: (() => void) | null = null;

/** Show the modal dialog. Only one is open at a time: opening another
 *  dismisses the current one first. */
export function openDialog<T>(options: DialogOptions<T>): Promise<T> {
  closeCurrent?.();
  return new Promise((resolve) => {
    const modal = $("modal");
    const bodyEl = $("modal-body");
    const actions = $("modal-actions");
    $("modal-title").textContent = options.title;
    bodyEl.replaceChildren(options.body);
    bodyEl.classList.toggle("pre", !!options.pre);
    modal.classList.toggle("wide", !!options.wide);

    // the page behind the dialog takes neither clicks nor the Tab key
    const behind = [...document.body.children].filter((el) => el !== modal && el instanceof HTMLElement);
    for (const el of behind) el.toggleAttribute("inert", true);

    let busy = false;
    const finish = (value: T) => {
      for (const el of behind) el.toggleAttribute("inert", false);
      modal.hidden = true;
      bodyEl.replaceChildren();
      actions.replaceChildren();
      document.removeEventListener("keydown", onKey);
      modal.removeEventListener("mousedown", onBackdrop);
      closeCurrent = null;
      resolve(value);
    };
    const press = async (value: T) => {
      if (busy) return;
      busy = true;
      try {
        if ((await options.beforeClose?.(value)) === false) return;
      } finally {
        busy = false;
      }
      finish(value);
    };
    const onKey = (ev: KeyboardEvent) => {
      if (ev.key === "Escape" && !busy) finish(options.dismiss);
    };
    const onBackdrop = (ev: MouseEvent) => {
      if (ev.target === modal && !busy) finish(options.dismiss);
    };

    // stable ids for the two conventional buttons: the dismissing one and
    // the first real action (a lone button counts as the action)
    const single = options.buttons.length === 1;
    let okAssigned = false;
    const buttons = options.buttons.map((b) => {
      const el = h(
        "button",
        { class: "btn" + (b.kind ? ` ${b.kind}` : ""), onclick: () => void press(b.value) },
        b.label,
      );
      if (b.value === options.dismiss && !single) el.id = "modal-cancel";
      else if (!okAssigned) {
        el.id = "modal-ok";
        okAssigned = true;
      }
      return el;
    });
    actions.replaceChildren(...buttons);

    closeCurrent = () => finish(options.dismiss);
    document.addEventListener("keydown", onKey);
    modal.addEventListener("mousedown", onBackdrop);
    modal.hidden = false;
    // Enter goes to the action the dialog is for. Where there is none (a
    // menu of equal choices) or it destroys something, it goes to the way
    // out instead: the focus ring must not single out a side action.
    const main = buttons.find((b) => b.classList.contains("primary"));
    const wayOut = buttons.find((b) => b.id === "modal-cancel");
    (main ?? wayOut ?? buttons[buttons.length - 1])?.focus();
  });
}

/** Close the open dialog as if it had been dismissed. */
export function dismissDialog() {
  closeCurrent?.();
}

export function confirmModal(title: string, body: string, okLabel: string, danger = false): Promise<boolean> {
  return openDialog({
    title,
    body,
    dismiss: false,
    buttons: [
      { label: t("common.cancel"), value: false },
      { label: okLabel, value: true, kind: danger ? "danger" : "primary" },
    ],
  });
}

/** A read-only report: preformatted, selectable text and a single button. */
export function infoModal(title: string, body: string): Promise<void> {
  return openDialog<void>({
    title,
    body,
    dismiss: undefined,
    wide: true,
    pre: true,
    buttons: [{ label: t("common.close"), value: undefined, kind: "primary" }],
  });
}
