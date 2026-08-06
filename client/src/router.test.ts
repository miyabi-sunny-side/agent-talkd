import { beforeEach, expect, it, vi } from "vitest";
import {
  currentRoute,
  navigate,
  onPopstate,
  parseRoute,
  serializeRoute,
  type Route,
} from "./router";

beforeEach(() => window.history.replaceState(null, "", "/"));

it("round-trips every route, including opaque pane ids", () => {
  const routes: Route[] = [
    { view: "registry" },
    { view: "letters" },
    { view: "agent", pane: "w1:p2" },
    // pane id は herdr 発行の opaque 文字列 — `/` や Unicode を含み得るので
    // path segment ではなく query に載せる。
    { view: "agent", pane: "pane/α:next?" },
    { view: "agent", pane: "%251" },
  ];
  for (const route of routes) {
    const url = serializeRoute(route);
    const [pathname, search = ""] = url.split("?");
    expect(parseRoute(pathname!, search ? `?${search}` : "")).toEqual(route);
  }
  // `/` を含む pane が path を割らない。
  expect(serializeRoute({ view: "agent", pane: "pane/α:next?" })).toBe(
    "/agent?pane=pane%2F%CE%B1%3Anext%3F",
  );
});

it("treats a missing or empty pane as an unknown route", () => {
  expect(parseRoute("/agent", "")).toBeNull();
  expect(parseRoute("/agent", "?pane=")).toBeNull();
  expect(parseRoute("/foo", "")).toBeNull();
});

it("normalizes an unknown path to the registry without losing the app", () => {
  window.history.replaceState(null, "", "/foo/bar");
  expect(currentRoute()).toEqual({ view: "registry" });
  expect(window.location.pathname).toBe("/");
});

it("pushes navigations but replaces tab switches", () => {
  const before = window.history.length;
  navigate({ view: "agent", pane: "w1:p2" });
  expect(window.location.pathname).toBe("/agent");
  expect(window.history.length).toBe(before + 1);

  // 同一 session の agent タブ切替は履歴を増やさない (Back は一覧へ)。
  navigate({ view: "agent", pane: "w1:p3" }, "replace");
  expect(new URLSearchParams(window.location.search).get("pane")).toBe("w1:p3");
  expect(window.history.length).toBe(before + 1);
});

it("reports the current route on popstate and stops after unsubscribe", () => {
  const seen: Route[] = [];
  const unsubscribe = onPopstate((route) => seen.push(route));

  window.history.replaceState(null, "", "/letters");
  window.dispatchEvent(new PopStateEvent("popstate"));
  expect(seen).toEqual([{ view: "letters" }]);

  unsubscribe();
  window.history.replaceState(null, "", "/");
  window.dispatchEvent(new PopStateEvent("popstate"));
  expect(seen).toHaveLength(1);
});

it("keeps currentRoute in sync with the address bar", () => {
  window.history.replaceState(null, "", "/agent?pane=w2%3Ap4");
  expect(currentRoute()).toEqual({ view: "agent", pane: "w2:p4" });
  vi.restoreAllMocks();
});
