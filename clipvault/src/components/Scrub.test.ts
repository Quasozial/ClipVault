import { describe, expect, it } from "vitest";
import { frameAt } from "./Scrub";

describe("frameAt", () => {
  it("maps the pointer across the card to frames 0..7", () => {
    expect(frameAt(0, 320)).toBe(0);
    expect(frameAt(39, 320)).toBe(0);
    expect(frameAt(40, 320)).toBe(1);
    expect(frameAt(319, 320)).toBe(7);
  });
  it("clamps outside the card and copes with zero width", () => {
    expect(frameAt(-5, 320)).toBe(0);
    expect(frameAt(400, 320)).toBe(7);
    expect(frameAt(10, 0)).toBe(0);
  });
});
