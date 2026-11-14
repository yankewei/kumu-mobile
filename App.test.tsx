// The Rust root tested from TypeScript: `nsh test` runs this file in a
// headless runner it builds with the crate, and `open(".")` starts the root
// (`nsh.json` "root").
import { expect, open, test } from "nsh/test";

test("opens on the pairing screen", async () => {
  const app = await open(".");
  const text = app.texts().join("\n");
  expect(text).toContain("Kumu");
  expect(text).toContain("配对链接");
});
