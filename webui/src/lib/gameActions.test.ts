import { describe, expect, it } from "vitest";
import { describeQualityCheck } from "./gameActions";
import type { QualityCheck } from "./ipc";

const check = (over: Partial<QualityCheck>): QualityCheck => ({
  selected: "uhd",
  launchWith: "uhd",
  onDisk: ["uhd"],
  files: { uhd: { present: 100, total: 100 } },
  ...over,
});

describe("describeQualityCheck", () => {
  it("is happy when the selected quality is whole and is what launches", () => {
    const r = describeQualityCheck(check({}), "Wuthering Waves");
    expect(r.type).toBe("success");
    expect(r.title).toBe("Wuthering Waves is set up for UHD");
  });

  it("says when the files are all there but a different quality launches", () => {
    const r = describeQualityCheck(check({ launchWith: "hd", onDisk: ["hd"] }), "Wuthering Waves");
    expect(r.type).toBe("warning");
    expect(r.title).toBe("Wuthering Waves launches at HD, not UHD");
  });

  it("counts the files of an unfinished quality", () => {
    const r = describeQualityCheck(
      check({ launchWith: "hd", onDisk: ["hd"], files: { hd: { present: 100, total: 100 }, uhd: { present: 40, total: 100 } } }),
      "Wuthering Waves",
    );
    expect(r.title).toBe("Wuthering Waves's UHD files are incomplete");
    expect(r.text).toContain("40 of 100 UHD files");
  });

  it("names the quality that is on disk when the selected one is not", () => {
    const r = describeQualityCheck(
      check({ launchWith: "hd", onDisk: ["hd"], files: { hd: { present: 100, total: 100 }, uhd: { present: 0, total: 100 } } }),
      "Wuthering Waves",
    );
    expect(r.title).toBe("Wuthering Waves isn't on UHD");
    expect(r.text).toContain("HD is what is on disk");
    expect(r.text).toContain("Apply downloads");
  });

  it("copes with nothing on disk at all", () => {
    const r = describeQualityCheck(check({ onDisk: [], files: {} }), "Wuthering Waves");
    expect(r.text).toBe("UHD is selected, but none of its files are on disk. Apply downloads them.");
  });
});
