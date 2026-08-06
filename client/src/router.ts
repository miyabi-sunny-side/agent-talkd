// History API による手書き router (DESIGN.md §2)。
//
// 外部 router 依存は追加しない。pane id は opaque (`%`・`/`・Unicode を
// 含み得る) なので path segment にせず、必ず URLSearchParams で query に
// 載せる。URL が唯一の画面情報源で、view state を別に持たない。

export type Route =
  { view: "registry" } | { view: "letters" } | { view: "agent"; pane: string };

/// URL を Route へ解釈する。未知の形は null (呼び出し側が `/` へ正規化)。
export function parseRoute(pathname: string, search: string): Route | null {
  if (pathname === "/") return { view: "registry" };
  if (pathname === "/letters") return { view: "letters" };
  if (pathname === "/agent") {
    const pane = new URLSearchParams(search).get("pane");
    if (pane !== null && pane !== "") return { view: "agent", pane };
  }
  return null;
}

export function serializeRoute(route: Route): string {
  switch (route.view) {
    case "registry":
      return "/";
    case "letters":
      return "/letters";
    case "agent": {
      const params = new URLSearchParams({ pane: route.pane });
      return `/agent?${params}`;
    }
  }
}

/// 現在 URL の Route。未知 path は `/` へ replaceState して registry を返す。
export function currentRoute(): Route {
  const parsed = parseRoute(window.location.pathname, window.location.search);
  if (parsed) return parsed;
  window.history.replaceState(null, "", "/");
  return { view: "registry" };
}

/// 画面遷移。一覧→詳細・→Letters は push、同 session のタブ切替は replace
/// (Back がタブ履歴を遡らず一覧へ戻るため。DESIGN.md §2)。
export function navigate(
  route: Route,
  mode: "push" | "replace" = "push",
): void {
  const url = serializeRoute(route);
  if (mode === "push") {
    window.history.pushState(null, "", url);
  } else {
    window.history.replaceState(null, "", url);
  }
}

/// popstate 購読。解除関数を返す。
export function onPopstate(handler: (route: Route) => void): () => void {
  const listener = (): void => handler(currentRoute());
  window.addEventListener("popstate", listener);
  return () => window.removeEventListener("popstate", listener);
}
