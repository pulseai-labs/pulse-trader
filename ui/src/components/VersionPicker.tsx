// The version picker (r4.s5.w2, A4) — a searchable combobox over the library
// catalog, replacing the Backtest Lab's native `<select>`.
//
// The catalog grows past a hundred versions (#218) and two strategies can share
// a name (#318), so the trader needs to type at a version rather than scroll to
// it, and needs a same-named group to say which strategy it is. The picker is
// display-only over `LibraryStrategy[]`: it invents no row the catalog does not
// carry, and every value on a row is the DTO's own string (the created date is
// w1's `dateUtc` — A2's `YYYY-MM-DD` with the `UTC` marker, r4.s5.w3 step 8).
//
// ARIA is the select-only combobox pattern: `role="combobox"` on the input, the
// popup a `role="listbox"` of `role="group"`s and `role="option"`s, focus held
// in the input with the active option on `aria-activedescendant` — options are
// never focus stops, so the screen's Tab order is what it was. Keys: ↓/↑ move
// the active option (clamped at either end), Enter selects it, Escape closes
// without selecting, Home/End jump to the first/last match, and typing filters
// case-insensitively on the strategy name, the version label (`v<n>`) and the
// 8-character short id.
//
// Only the filtered list is rendered: a 120-version catalog costs 120 `li`s
// while nothing is typed, and one strategy's worth once a name is typed — with
// no virtualisation library in the bundle.

import { useEffect, useMemo, useRef, useState } from "react";
import type { ChangeEvent, KeyboardEvent, MouseEvent } from "react";

import type { LibraryStrategy, LibraryVersion } from "../bindings";
import { dateUtc } from "../format/time";
import { provenanceLine } from "./provenance";

/** How much of an id the picker shows: the Library's own short-id convention. */
const SHORT_ID_LEN = 8;

/** The popup's DOM ids. One picker owns the screen's `bt-version` input, so
 * these are unique on the page and `aria-activedescendant` has a stable name
 * for whichever option is active. */
const LISTBOX_ID = "bt-version-listbox";
const OPTION_ID_PREFIX = "bt-version-opt-";
const GROUP_ID_PREFIX = "bt-version-group-";

/** One selectable version, pre-rendered: the label, the id's short form, and
 * the one line the trader reads. */
export interface VersionOption {
  readonly id: string;
  readonly label: string;
  readonly shortId: string;
  readonly text: string;
}

/** One strategy's group in the popup. */
export interface VersionGroup {
  readonly id: string;
  readonly name: string;
  /** The group's header: the strategy's name, disambiguated when the name is
   * carried by more than one strategy (#318). */
  readonly header: string;
  readonly options: readonly VersionOption[];
}

/**
 * The strategy names carried by more than one strategy — #318's disambiguator
 * trigger, found in one pass so a large catalog costs one lookup per strategy.
 */
function collidingNames(strategies: readonly LibraryStrategy[]): Set<string> {
  const seen = new Set<string>();
  const colliding = new Set<string>();
  for (const strategy of strategies) {
    if (seen.has(strategy.name)) {
      colliding.add(strategy.name);
    }
    seen.add(strategy.name);
  }
  return colliding;
}

/**
 * Build the picker's groups from the catalog: one group per strategy in catalog
 * order, versions in their `version_tree` order, each option carrying `v<n>`,
 * the created date, the short id and the Library's provenance line.
 *
 * A name carried by two or more strategies is ambiguous, so EVERY group with
 * that name adds its strategy's created date and short id — disambiguating one
 * side of a collision and not the other would leave the trader unable to tell
 * which group is which.
 */
export function buildVersionGroups(strategies: readonly LibraryStrategy[]): VersionGroup[] {
  const colliding = collidingNames(strategies);
  return strategies.map((strategy) => {
    const header = colliding.has(strategy.name)
      ? `${strategy.name} · ${dateUtc(strategy.createdAt)} · ${strategy.id.slice(0, SHORT_ID_LEN)}`
      : strategy.name;
    return {
      id: strategy.id,
      name: strategy.name,
      header,
      options: strategy.versions.map((version: LibraryVersion, index: number) => {
        const label = `v${index + 1}`;
        const shortId = version.id.slice(0, SHORT_ID_LEN);
        const pinned = strategy.pinnedVersionId === version.id;
        return {
          id: version.id,
          label,
          shortId,
          // r4.s5.w3 (spec step 8): the date is w1's own formatter now — A2's
          // `YYYY-MM-DD` with its `UTC` marker — not a local slice of the
          // RFC3339 text.
          text:
            `${label} · ${dateUtc(version.createdAt)} · ${shortId} · ` +
            `${provenanceLine(version)}${pinned ? " · pinned" : ""}`,
        };
      }),
    };
  });
}

export interface VersionPickerProps {
  /** The catalog's strategies, exactly as `libraryOverview` served them. */
  readonly strategies: readonly LibraryStrategy[];
  /** The selected version's id, or `null` while nothing is selected. */
  readonly value: string | null;
  readonly onChange: (versionId: string) => void;
  /** The Lab's latch: while either operation is in flight the picker is inert,
   * exactly as the `<select>` was. */
  readonly disabled: boolean;
}

export function VersionPicker({ strategies, value, onChange, disabled }: VersionPickerProps) {
  const groups = useMemo(() => buildVersionGroups(strategies), [strategies]);
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const root = useRef<HTMLDivElement | null>(null);

  /** The filtered view: matching groups in catalog order with their options'
   * indexes in the flat list the keyboard walks. Built in one pass so the
   * render never searches for an option's position. */
  const view = useMemo(() => {
    const needle = query.trim().toLowerCase();
    const flat: VersionOption[] = [];
    const entries: Array<{
      group: VersionGroup;
      rows: Array<{ option: VersionOption; index: number }>;
    }> = [];
    for (const group of groups) {
      const matched =
        needle === "" || group.name.toLowerCase().includes(needle)
          ? group.options
          : group.options.filter(
              (option) =>
                option.label.toLowerCase().includes(needle) ||
                option.shortId.toLowerCase().includes(needle),
            );
      if (matched.length === 0) {
        continue;
      }
      const rows = matched.map((option) => {
        const index = flat.length;
        flat.push(option);
        return { option, index };
      });
      entries.push({ group, rows });
    }
    return { flat, entries };
  }, [groups, query]);

  const selected = useMemo(() => {
    for (const group of groups) {
      const option = group.options.find((candidate) => candidate.id === value);
      if (option !== undefined) {
        return option;
      }
    }
    return null;
  }, [groups, value]);

  /** The input shows the query while the trader is typing and the selected
   * option's own line otherwise — so what is selected is always readable. */
  const displayed = open && query !== "" ? query : (selected?.text ?? "");

  // The active option is scrolled into view as it moves. `scrollIntoView` is
  // optional on purpose: jsdom does not implement it.
  useEffect(() => {
    if (!open) {
      return;
    }
    document
      .getElementById(`${OPTION_ID_PREFIX}${active}`)
      ?.scrollIntoView?.({ block: "nearest" });
  }, [open, active]);

  // A busy screen closes the popup rather than leaving it hovering over a
  // disabled input.
  useEffect(() => {
    if (disabled) {
      setOpen(false);
      setQuery("");
    }
  }, [disabled]);

  function selectedIndex(): number {
    const index = view.flat.findIndex((option) => option.id === value);
    return index < 0 ? 0 : index;
  }

  function openList() {
    setQuery("");
    setActive(selectedIndex());
    setOpen(true);
  }

  function closeList() {
    setOpen(false);
    setQuery("");
    setActive(0);
  }

  function commit(option: VersionOption) {
    onChange(option.id);
    closeList();
  }

  function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
    if (disabled) {
      return;
    }
    switch (event.key) {
      case "ArrowDown":
        event.preventDefault();
        if (open) {
          setActive((current) => Math.min(current + 1, Math.max(view.flat.length - 1, 0)));
        } else {
          openList();
        }
        break;
      case "ArrowUp":
        event.preventDefault();
        if (open) {
          setActive((current) => Math.max(current - 1, 0));
        } else {
          openList();
        }
        break;
      case "Home":
        // The jumps stay inside the CURRENT filtered list: the query is what
        // the trader narrowed with, and Home/End do not clear it.
        event.preventDefault();
        setActive(0);
        setOpen(true);
        break;
      case "End":
        event.preventDefault();
        setActive(Math.max(view.flat.length - 1, 0));
        setOpen(true);
        break;
      case "Enter": {
        const option = open ? view.flat[active] : undefined;
        if (option !== undefined) {
          event.preventDefault();
          commit(option);
        }
        break;
      }
      case "Escape":
        if (open) {
          event.preventDefault();
          closeList();
        }
        break;
      default:
        break;
    }
  }

  function onQueryChange(event: ChangeEvent<HTMLInputElement>) {
    if (disabled) {
      return;
    }
    setQuery(event.target.value);
    setActive(0);
    setOpen(true);
  }

  function onOptionMouseDown(event: MouseEvent<HTMLLIElement>) {
    // Keep focus in the input: a real mousedown would blur it, and a blur closes
    // the popup before the click that selects.
    event.preventDefault();
  }

  return (
    <div className="vp-root" ref={root}>
      <input
        id="bt-version"
        className="bt-select vp-input"
        type="text"
        role="combobox"
        value={displayed}
        disabled={disabled}
        autoComplete="off"
        spellCheck={false}
        placeholder="Search versions…"
        aria-autocomplete="list"
        aria-expanded={open}
        aria-controls={open ? LISTBOX_ID : undefined}
        aria-activedescendant={
          open && view.flat[active] !== undefined ? `${OPTION_ID_PREFIX}${active}` : undefined
        }
        onChange={onQueryChange}
        onKeyDown={onKeyDown}
        onFocus={() => {
          if (!disabled && !open) {
            openList();
          }
        }}
        onClick={() => {
          if (!disabled && !open) {
            openList();
          }
        }}
        onBlur={closeList}
      />
      {open && (
        <ul role="listbox" id={LISTBOX_ID} aria-label="Strategy versions" className="vp-list">
          {view.flat.length === 0 && (
            <li role="presentation" className="vp-empty">
              No versions match “{query.trim()}”
            </li>
          )}
          {view.entries.map(({ group, rows }) => (
            <li key={group.id} role="group" aria-labelledby={`${GROUP_ID_PREFIX}${group.id}`}>
              <div id={`${GROUP_ID_PREFIX}${group.id}`} className="vp-group">
                {group.header}
              </div>
              <ul role="presentation" className="vp-options">
                {rows.map(({ option, index }) => (
                  <li
                    key={option.id}
                    id={`${OPTION_ID_PREFIX}${index}`}
                    role="option"
                    aria-selected={option.id === value}
                    className={`vp-option${index === active ? " is-active" : ""}`}
                    onMouseDown={onOptionMouseDown}
                    onClick={() => commit(option)}
                  >
                    {option.text}
                  </li>
                ))}
              </ul>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}
