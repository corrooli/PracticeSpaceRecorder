import { it, expect, vi } from "vitest";
import { FakeWS, loadPage } from "./setup";
import { state } from "./fixtures";

it("deep link opens the sheet once state arrives, screenshot mode, wss on https", async () => {
  loadPage();
  window.history.replaceState(null, "", "/?shot=1#setup");
  (document as any).fonts = { ready: Promise.resolve() };
  vi.useFakeTimers();
  await import("../src/main");
  expect(document.body.classList.contains("shot")).toBe(true);
  const ws = FakeWS.last();
  ws.open();
  vi.advanceTimersByTime(300);
  expect(document.getElementById("sheet")!.classList.contains("hidden")).toBe(true);
  ws.msg({ t: "hello", privileged: true });
  ws.msg(state());
  vi.advanceTimersByTime(150);
  expect(document.getElementById("sheet")!.textContent).toContain("REMOTE CONTROL");
  vi.useRealTimers();
});

it("uses wss behind https", async () => {
  vi.resetModules();
  loadPage();
  const loc = window.location;
  Object.defineProperty(window, "location", { value: { ...loc, protocol: "https:", host: "rec.example:443", search: "", hash: "" }, configurable: true });
  await import("../src/main");
  expect(FakeWS.last().url).toBe("wss://rec.example:443/ws");
  Object.defineProperty(window, "location", { value: loc, configurable: true });
});
