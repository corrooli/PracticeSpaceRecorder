import { describe, it, expect } from "vitest";
import { KINDS, kindInfo, newDestination, parseShareUrl } from "../src/destinations";
import { dest } from "./fixtures";

describe("destination kinds", () => {
  it("every kind has a face", () => {
    for (const k of KINDS) {
      expect(k.icon).toContain("<svg");
      expect(k.title).toBe(k.title.toUpperCase());
      expect(kindInfo(k.kind)).toBe(k);
    }
    expect(kindInfo("unsupported").title).toBe("NEWER KIND");
    expect(kindInfo("telegram" as any).label).toBe("Destination");
  });

  it("new destinations get a name nobody else has", () => {
    expect(newDestination("folder", [], "a")).toEqual({ id: "a", kind: "folder", label: "Local", path: "", smb_url: "", mount_cmd: "", enabled: true });
    const taken = [dest({ label: "NAS" }), dest({ label: "NAS 2" })];
    expect(newDestination("share", taken, "b").label).toBe("NAS 3");
    expect(newDestination("share", [dest({ label: "NAS 2" })], "c").label).toBe("NAS");
  });

  it("share addresses split like dest.rs does", () => {
    expect(parseShareUrl("smb://oliver@NasSSD.internal/Oliver/Test/")).toEqual({ host: "nasssd.internal", share: "Oliver", sub: ["Test"] });
    expect(parseShareUrl("  SMB://h:445/Music%20Stuff/a/./../b  ")).toEqual({ host: "h", share: "Music Stuff", sub: ["a", "b"] });
    expect(parseShareUrl("afp://h/S")).toEqual({ host: "h", share: "S", sub: [] });
    expect(parseShareUrl("smb://h/bad%E0%A4")).toEqual({ host: "h", share: "bad%E0%A4", sub: [] });
    expect(parseShareUrl("smb://h/")).toBeNull();
    expect(parseShareUrl("smb:///Share")).toBeNull();
    expect(parseShareUrl("/Volumes/x")).toBeNull();
    expect(parseShareUrl("http://h/x")).toBeNull();
  });
});
