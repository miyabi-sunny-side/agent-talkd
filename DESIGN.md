---
version: alpha
name: Sumi / agent-talkd
description: >
  agent-talkd project overrides for the Sumi design system. This file records
  the implemented read-only observation UI identity, state colors, and components.
colors:
  ink: "#171714"
  ink-raised: "#1d1d19"
  paper: "#e9e4d8"
  paper-muted: "#a8a294"
  line: "#3a3933"
  kaki: "#d66f3d"
  teal: "#62b7a5"
  ochre: "#c9a24e"
  danger: "#e68269"
---

# agent-talkd — Sumi Project Overrides

## Overview

The observation UI is a compact, read-only operational surface for agents in the
current herdr. Its Sumi identity uses a dark ink ground, warm paper text,
and a restrained persimmon accent (`kaki`) for the product mark, section index,
focus ring, loader, and actions. The page has one theme and no theme toggle.
Registry rows lead to a dedicated Screen view, while Letters is a separate
mailbox timeline; terminal content remains the dominant element when selected.

## Domain colors

- **Agent idle:** teal. It appears only on the row stroke, status dot and status
  label, and the empty-registry mark.
- **Agent busy:** ochre. It appears only on the row stroke, status dot and status
  label. The literal `busy` label carries the same information without relying
  on color.
- **Failure:** `danger` red is reserved for the failed output, error mark, and error
  copy. It is not an agent state.

The frontmatter color names and values match the CSS references in
`client/src/styles.css`; all entries are custom properties. State meaning is
reinforced by text and placement.

## Domain components

- **Masthead:** a small `HERDR / LOCAL BROKER` eyebrow, `agent talk` title, and
  square `話` seal establish identity without competing with registry content.
- **Registry heading:** section index `一`, title, and an `aria-live` output show
  loading, failure, empty, or agent-count feedback.
- **Agent row:** a compact record of runtime name, full working directory,
  herdr location, pane ID, and idle/busy status. Long name and path values
  truncate visually; the full cwd remains in the element title. The whole row
  is a keyboard-operable action opening that agent's Screen view. A four-pixel
  state stroke and labeled status dot repeat state.
- **Screen:** a dedicated detail surface with the selected agent identity,
  manual refresh, a two-second visible-only polling cadence, and a monospace
  plain-text terminal. The terminal occupies the largest visual area and scrolls
  without forcing the surrounding page wider. A failed refresh dims the last
  successful capture instead of replacing it; an initial failure shows retry.
- **Letters:** a mailbox selector and chronological timeline. Each entry labels
  its direction as `IN` or `OUT`; teal and kaki reinforce but never replace the
  text. The view does not poll: manual refresh appends events through the ID
  cursor without consuming the mailbox.
- **Loading, empty, and failure states:** the registry area keeps a stable
  bounded surface. Failure alone exposes an explicit retry button; the page
  does not poll or provide a manual refresh during successful display.
- **Footer:** `READ ONLY` and the backend label state the operational boundary.

## Constraints

- Keep every view read-only. Screen capture and letter history are observation
  only; do not add terminal input, message composition, or recovery controls
  before the Port-3 human identity gate can authorize those actions.
- Render terminal and letter bodies through Svelte text bindings only. Do not
  interpret terminal escapes as markup or use raw HTML insertion.
- Never use idle/busy colors for general actions or decoration.
- Keep agent identity, herdr coordinates, working directory, and textual state
  visible as one record. The narrow layout may wrap coordinates below identity,
  but must not separate their meaning.
- Loading and row-entry motion must collapse to effectively static rendering
  when reduced motion is requested.
- Maintain keyboard-visible focus on row, navigation, refresh, selector, and
  retry actions, and live-region feedback for asynchronous status changes.
