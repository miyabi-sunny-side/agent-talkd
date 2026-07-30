---
version: alpha
name: Sumi / agent-talkd
description: >
  agent-talkd project overrides for the Sumi design system. This file records
  the implemented registry status page identity, state colors, and components.
colors:
  ink: "#171714"
  paper: "#e9e4d8"
  paper-muted: "#a8a294"
  line: "#3a3933"
  kaki: "#d66f3d"
  teal: "#62b7a5"
  ochre: "#c9a24e"
  error: "#e68269"
---

# agent-talkd — Sumi Project Overrides

## Overview

The status page is a compact, read-only operational registry for agents in the
current tmux server. Its Sumi identity uses a dark ink ground, warm paper text,
and a restrained persimmon accent (`kaki`) for the product mark, section index,
focus ring, loader, and retry action. The page has one theme and no theme toggle.

## Domain colors

- **Agent idle:** teal. It appears only on the row stroke, status dot and status
  label, and the empty-registry mark.
- **Agent busy:** ochre. It appears only on the row stroke, status dot and status
  label. The literal `busy` label carries the same information without relying
  on color.
- **Failure:** muted red is reserved for the failed output, error mark, and error
  copy. It is not an agent state.

The frontmatter color names and values match the CSS references in
`client/src/styles.css`; `error` is a fixed reference while the other entries
are custom properties. State meaning is reinforced by text and placement.

## Domain components

- **Masthead:** a small `TMUX / LOCAL BROKER` eyebrow, `agent talk` title, and
  square `話` seal establish identity without competing with registry content.
- **Registry heading:** section index `一`, title, and an `aria-live` output show
  loading, failure, empty, or agent-count feedback.
- **Agent row:** a compact record of runtime name, full working directory,
  tmux location, pane ID, and idle/busy status. Long name and path values
  truncate visually; the full cwd remains in the element title. A four-pixel
  state stroke and labeled status dot repeat state without making the row an
  action target.
- **Loading, empty, and failure states:** the registry area keeps a stable
  bounded surface. Failure alone exposes an explicit retry button; the page
  does not poll or provide a manual refresh during successful display.
- **Footer:** `READ ONLY` and `同一 tmux server` state the operational boundary.

## Constraints

- Keep the registry read-only. Do not add terminal input, message composition,
  recovery controls, screen capture, or letter history to this status page.
- Never use idle/busy colors for general actions or decoration.
- Keep agent identity, tmux coordinates, working directory, and textual state
  visible as one record. The narrow layout may wrap coordinates below identity,
  but must not separate their meaning.
- Loading and row-entry motion must collapse to effectively static rendering
  when reduced motion is requested.
- Maintain keyboard-visible focus on the retry action and live-region feedback
  for asynchronous status changes.
