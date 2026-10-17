import { describe, expect, it } from "vitest";
import { hasKey, LANGUAGES, setLanguagePreference, t } from "../lib/i18n";

describe("translations", () => {
  it("lists English first", () => {
    expect(LANGUAGES[0]?.code).toBe("en");
  });

  it("fills placeholders", () => {
    setLanguagePreference("en");
    expect(t("format.hm", { h: 2, m: 5 })).toBe("2h 5m");
    expect(hasKey("format.nope")).toBe(false);
  });

  it("picks the plural form from count", () => {
    setLanguagePreference("en");
    expect(t("setup.status.picked", { count: 1 })).toBe("1 game selected");
    expect(t("setup.status.picked", { count: 3 })).toBe("3 games selected");
    expect(hasKey("setup.status.picked")).toBe(true);
  });

  it("falls back to English for an unknown language", () => {
    setLanguagePreference("xx-not-shipped");
    expect(t("common.cancel")).toBe("Cancel");
    setLanguagePreference("en");
  });
});
