import { expect, it } from "vitest";
import { appendPath, imageProblem, MAX_IMAGE_BYTES } from "./image";

it("appends the path as its own line without changing existing text", () => {
  const path = "/home/u/.cache/agent-talk/images/image-a.png";
  expect(appendPath("", path)).toBe(`${path}\n`);
  expect(appendPath("調べて", path)).toBe(`調べて\n${path}\n`);
  expect(appendPath("一行目\n", path)).toBe(`一行目\n${path}\n`);
  expect(appendPath("  前\n\n", path)).toBe(`  前\n\n${path}\n`);
  expect(appendPath(`${path}\n`, "/b.jpg")).toBe(`${path}\n/b.jpg\n`);
});

it("accepts supported screenshots and photos up to the size limit", () => {
  for (const type of ["image/png", "image/jpeg", "image/webp"])
    expect(imageProblem({ type, size: MAX_IMAGE_BYTES })).toBe("");
});

it("explains unsupported, empty and oversized files before uploading", () => {
  for (const type of [
    "image/heic",
    "image/heif",
    "image/gif",
    "",
    "text/plain",
  ])
    expect(imageProblem({ type, size: 10 })).toMatch(/PNG・JPEG・WebP/);
  expect(imageProblem({ type: "image/heic", size: 10 })).toMatch(/JPEG/);
  expect(imageProblem({ type: "image/png", size: 0 })).toMatch(/空/);
  expect(
    imageProblem({ type: "image/jpeg", size: MAX_IMAGE_BYTES + 1 }),
  ).toMatch(/20 MiB/);
});
