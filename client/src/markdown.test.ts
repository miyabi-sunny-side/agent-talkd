import { expect, it } from "vitest";
import { renderMarkdown } from "./markdown";

function content(source: string) {
  const element = document.createElement("div");
  element.innerHTML = renderMarkdown(source);
  return element;
}

it("renders reports with paragraphs, lists, emphasis, links, and tables", () => {
  const element = content(
    "# 報告\n\n**完了**と*補足*\n次の行\n\n- 一つ\n- 二つ\n\n1. 手順\n\n[資料](https://example.com/report?q=1&lang=ja)\n\n|項目|結果|\n|---|---|\n|確認|成功|",
  );
  expect(element.querySelector("h1")?.textContent).toBe("報告");
  expect(element.querySelector("strong")?.textContent).toBe("完了");
  expect(element.querySelector("em")?.textContent).toBe("補足");
  expect(element.querySelector("p br")).not.toBeNull();
  expect(element.querySelectorAll("ul li")).toHaveLength(2);
  expect(element.querySelector("ol li")?.textContent).toBe("手順");
  expect(element.querySelector("a")?.getAttribute("href")).toBe(
    "https://example.com/report?q=1&lang=ja",
  );
  expect(element.querySelector("td")?.textContent).toBe("確認");
});

it("preserves code text, indentation, blank lines, and HTML characters", () => {
  const code = '  <script>alert("x")</script>\n\n\tconst a = 1;\n';
  const element = content("```html\n" + code + "```\n\n`<img src=x>`");
  expect(element.querySelector("pre code")?.textContent).toBe(code);
  expect(element.querySelector("p code")?.textContent).toBe("<img src=x>");
  expect(element.querySelector("script, img")).toBeNull();
});

it("keeps raw HTML visible without executable elements or network loads", () => {
  const source =
    '<script>alert(1)</script>\n\n<img src="https://example.com/pixel" onerror="alert(1)">\n\n<iframe src="https://example.com"></iframe>\n\n<a href="javascript:alert(1)">raw</a>';
  const element = content(source);
  expect(element.textContent).toContain("<script>alert(1)</script>");
  expect(element.textContent).toContain(
    '<img src="https://example.com/pixel" onerror="alert(1)">',
  );
  expect(element.textContent).toContain(
    '<iframe src="https://example.com"></iframe>',
  );
  expect(element.querySelector("script, img, iframe, a")).toBeNull();
});

it.each([
  "javascript:alert%281%29",
  "JaVaScRiPt:alert%281%29",
  "javascript&#58;alert%281%29",
  "jav&#x61;script:alert%281%29",
  "data:text/html,payload",
  "vbscript:payload",
  "file:///etc/passwd",
  "/absolute/path",
  "//example.com/path",
])("shows an unsafe or local link as its label: %s", (uri) => {
  const element = content(`[**資料**](${uri})`);
  expect(element.querySelector("a")).toBeNull();
  expect(element.querySelector("strong")?.textContent).toBe("資料");
});

it("allows explicit web and email links", () => {
  const element = content(
    "[web](http://example.com) [mail](mailto:person@example.com)",
  );
  expect(
    Array.from(element.querySelectorAll("a"), (a) => a.getAttribute("href")),
  ).toEqual(["http://example.com", "mailto:person@example.com"]);
});

it("shows image alternatives without loading Markdown images", () => {
  const element = content(
    '![図の説明](https://example.com/pixel "図")\n\n[![リンク内の画像](https://example.com/image)](https://example.com/report)',
  );
  expect(element.querySelector("img")).toBeNull();
  expect(element.textContent).toContain("図の説明");
  expect(element.querySelector("a")?.textContent).toBe("リンク内の画像");
});
