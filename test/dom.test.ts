import { describe, it, expect, vi } from "vitest";
import { h, setText, toggleClass, clear, led, setLed, fmtBytes, fmtClock, fmtDur, dB, noFocus } from "../src/dom";

describe("h()", () => {
  it("builds elements with attributes, props, handlers and children", () => {
    const onClick = vi.fn();
    const el = h("button", { class: "key on", onclick: onClick, title: "hi", disabled: true, tabindex: 3, hidden: false, "data-x": null, foo: undefined },
      "a", 1, null, undefined, false, [h("i"), ["b", [h("b")]]]);
    expect(el.className).toBe("key on");
    expect(el.title).toBe("hi");
    expect(el.disabled).toBe(true);
    expect(el.tabIndex).toBe(3);
    expect(el.hasAttribute("hidden")).toBe(false);
    expect(el.hasAttribute("data-x")).toBe(false);
    expect(el.textContent).toBe("a1b");
    expect(el.querySelectorAll("i, b").length).toBe(2);
    const b = h("button", { onclick: onClick });
    b.click();
    expect(onClick).toHaveBeenCalledOnce();
  });

  it("sets html, string attributes and boolean attributes", () => {
    expect(h("div", { html: "<b>x</b>" }).innerHTML).toBe("<b>x</b>");
    expect(h("div", { "aria-label": "z" }).getAttribute("aria-label")).toBe("z");
    expect(h("div", { "data-on": true }).getAttribute("data-on")).toBe("");
    expect(h("span", null, "t").textContent).toBe("t");
  });
});

describe("helpers", () => {
  it("setText only writes on change", () => {
    const el = h("div", null, "same");
    const spy = vi.spyOn(el, "textContent", "set");
    setText(el, "same");
    expect(spy).not.toHaveBeenCalled();
    setText(el, "new");
    expect(el.textContent).toBe("new");
  });

  it("toggleClass, clear", () => {
    const el = h("div", null, h("i"), h("i"));
    toggleClass(el, "x", true);
    toggleClass(el, "x", true);
    expect(el.className).toBe("x");
    toggleClass(el, "x", false);
    expect(el.className).toBe("");
    clear(el);
    expect(el.childNodes.length).toBe(0);
  });

  it("LEDs keep their size class", () => {
    expect(led().className).toBe("led led-off ");
    const l = led("green", "led-lg");
    setLed(l, "red", true);
    expect(l.className).toBe("led led-red blink led-lg");
    const s = led("green", "led-sm");
    setLed(s, "amber");
    expect(s.className).toBe("led led-amber led-sm");
    setLed(s, "amber");
    expect(s.className).toBe("led led-amber led-sm");
  });

  it("formats bytes", () => {
    expect(fmtBytes(0)).toBe("-");
    expect(fmtBytes(512)).toBe("512 B");
    expect(fmtBytes(2048)).toBe("2 KB");
    expect(fmtBytes(5 * 1024 ** 3)).toBe("5.0 GB");
    expect(fmtBytes(50 * 1024 ** 3)).toBe("50 GB");
    expect(fmtBytes(3 * 1024 ** 5)).toBe("3072 TB");
  });

  it("formats clocks and durations", () => {
    expect(fmtClock(-5)).toBe("00:00:00");
    expect(fmtClock(3723.45, true)).toBe("01:02:03.4");
    expect(fmtDur(42)).toBe("42s");
    expect(fmtDur(75)).toBe("1:15");
    expect(fmtDur(7380)).toBe("2h 03m");
  });

  it("dB", () => {
    expect(dB(1)).toBe(0);
    expect(dB(0)).toBe(-Infinity);
    expect(dB(0.5)).toBeCloseTo(-6.02, 2);
  });

  it("noFocus stops buttons from taking focus", () => {
    const b = noFocus(h("button"));
    expect(b.getAttribute("tabindex")).toBe("-1");
    const e = new MouseEvent("mousedown", { cancelable: true });
    b.dispatchEvent(e);
    expect(e.defaultPrevented).toBe(true);
  });
});
