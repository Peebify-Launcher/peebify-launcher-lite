// ------------ About Credits ------------
// The top of the About tab: the launcher's name, version and links, then who made Peebify and XXMI, each with a
// GitHub avatar and a Profile button.
import { useState, type ReactNode } from "react";
import { ArrowUpRight, Globe } from "lucide-react";
import { openExternal } from "../../lib/tauri";
import { PEEBIFY_DISCORD_URL, PEEBIFY_SITE_URL, XXMI_URL } from "../../data/settings";
import { useUiStore } from "../../store/uiStore";
import { BRAND_PATHS } from "../home/Home";
import { useT } from "../../lib/useT";

const GITHUB_PATH =
  "M12 .5C5.65.5.5 5.65.5 12a11.5 11.5 0 0 0 7.86 10.92c.58.1.79-.25.79-.56v-2c-3.2.7-3.88-1.37-3.88-1.37-.53-1.33-1.28-1.69-1.28-1.69-1.05-.72.08-.7.08-.7 1.16.08 1.77 1.19 1.77 1.19 1.03 1.77 2.7 1.26 3.36.96.1-.75.4-1.26.73-1.55-2.55-.29-5.24-1.28-5.24-5.69 0-1.26.45-2.29 1.19-3.1-.12-.29-.52-1.46.11-3.05 0 0 .97-.31 3.17 1.18a11 11 0 0 1 5.77 0c2.2-1.49 3.17-1.18 3.17-1.18.63 1.59.23 2.76.11 3.05.74.81 1.19 1.84 1.19 3.1 0 4.42-2.69 5.39-5.26 5.68.41.36.78 1.06.78 2.14v3.17c0 .31.21.67.8.56A11.5 11.5 0 0 0 23.5 12C23.5 5.65 18.35.5 12 .5Z";

interface Person {
  name: string;
  github: string;
  avatar: string;
}

const DEVELOPER: Person = { name: "Acheuy", github: "Cheu3172", avatar: "/icons/credits/acheuy.webp" }; // i18n-ignore
const CONTRIBUTORS: Person[] = [
  { name: "Blackwolf", github: "blackwolf660", avatar: "/icons/credits/blackwolf.webp" }, // i18n-ignore
  { name: "mrflashstudio", github: "mrflashstudio", avatar: "/icons/credits/mrflashstudio.webp" }, // i18n-ignore
];
const XXMI_MAKERS: Person[] = [
  { name: "SpectrumQT", github: "SpectrumQT", avatar: "/icons/credits/spectrumqt.webp" }, // i18n-ignore
  { name: "leotorrez", github: "leotorrez", avatar: "/icons/credits/leotorrez.webp" }, // i18n-ignore
];

const ART_MASK = "linear-gradient(90deg, transparent 0%, rgba(0,0,0,0.55) 38%, #000 70%)";

const LINK_BUTTON =
  "accent-grad inline-flex h-[34px] select-none items-center gap-2 rounded-ui px-[14px] text-[13px] font-medium text-(--on-accent) transition duration-150 hover:brightness-95 active:scale-[0.97]";

function Mark({ path, size }: { path: string; size: number }) {
  return (
    <svg viewBox="0 0 24 24" width={size} height={size} fill="currentColor" aria-hidden="true">
      <path d={path} />
    </svg>
  );
}

function Avatar({ person, size }: { person: Person; size: number }) {
  const [failed, setFailed] = useState(false);
  if (failed) {
    return (
      <span
        aria-hidden="true"
        className="grid shrink-0 place-items-center rounded-full bg-white/[0.08] font-semibold text-white/70"
        style={{ width: size, height: size, fontSize: size * 0.38 }}
      >
        {person.name.slice(0, 1).toUpperCase()}
      </span>
    );
  }
  return (
    <img
      src={person.avatar}
      alt=""
      draggable={false}
      onError={() => setFailed(true)}
      className="shrink-0 rounded-full object-cover"
      style={{
        width: size,
        height: size,
        boxShadow: "0 0 0 1px rgba(255,255,255,.14), 0 8px 20px -8px rgba(0,0,0,.7)",
      }}
    />
  );
}

function PersonCard({ person }: { person: Person }) {
  const t = useT();
  return (
    <div
      className="group flex items-center gap-[14px] rounded-[11px] border border-white/[0.08] bg-white/[0.05] px-[18px] py-4 transition duration-150 hover:-translate-y-px hover:border-white/[0.18] hover:bg-white/[0.08]"
    >
      <Avatar person={person} size={48} />
      <span className="min-w-0 flex-1 truncate text-[14.5px] font-semibold text-white">{person.name}</span>
      <button
        type="button"
        onClick={() => void openExternal(`https://github.com/${person.github}`)}
        aria-label={t("settings.about.profileOf", { name: person.name })}
        className="inline-flex h-[30px] shrink-0 items-center gap-[7px] rounded-ui border border-white/15 bg-white/[0.05] px-[11px] text-[12.5px] font-medium text-white transition duration-150 hover:border-white/30 hover:bg-white/[0.12] active:scale-[0.97] group-hover:border-white/25 group-hover:bg-white/[0.09]"
      >
        <Mark path={GITHUB_PATH} size={14} />
        {t("settings.about.profile")}
      </button>
    </div>
  );
}

function Section({ label, aside, children }: { label: string; aside?: ReactNode; children: ReactNode }) {
  return (
    <section>
      <div className="mb-[10px] flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 px-1">
        <h3 className="text-[12px] font-semibold uppercase tracking-[0.07em] text-white/55">{label}</h3>
        {aside}
      </div>
      {children}
    </section>
  );
}

export function AboutHero() {
  const t = useT();
  const version = useUiStore((s) => s.appVersion);
  return (
    <section
      aria-label="Peebify Launcher" // i18n-ignore
      className="relative flex min-h-[176px] flex-wrap items-end justify-between gap-x-7 gap-y-3 overflow-hidden rounded-[11px] border border-white/10 p-[22px]"
      style={{
        background: "rgba(18,18,22,0.72)",
        backdropFilter: "blur(30px) saturate(180%)",
        WebkitBackdropFilter: "blur(30px) saturate(180%)",
      }}
    >
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-y-0 right-0 w-[62%]"
        style={{ maskImage: ART_MASK, WebkitMaskImage: ART_MASK }}
      >
        <img
          src="/icons/about-banner.jpg"
          alt=""
          draggable={false}
          className="h-full w-full -scale-x-100 object-cover object-[40%_62%]"
        />
      </div>
      <div
        aria-hidden="true"
        className="pointer-events-none absolute inset-0"
        style={{ background: "linear-gradient(180deg, rgba(10,12,16,0) 35%, rgba(10,12,16,0.72) 100%)" }}
      />
      <div className="relative flex min-w-0 items-center gap-[18px]">
        <img
          src="/icons/app.png"
          alt=""
          className="h-[72px] w-[72px] shrink-0 rounded-[16px] bg-black/30 object-contain p-1.5"
          style={{ boxShadow: "0 0 0 1px rgba(255,255,255,.12), 0 8px 20px -8px rgba(0,0,0,.7)" }}
        />
        <div className="min-w-0">
          <h2
            className="text-[28px] font-semibold leading-[1.15] tracking-[-0.01em] text-white"
            style={{ fontFamily: "var(--font-display)" }}
          >
            Peebify Launcher
          </h2>
          {/* i18n-ignore */}
          {version && (
            <p className="mt-[6px] text-[13px] leading-[18px] tabular-nums text-white/70">
              {t("settings.about.version", { version })}
            </p>
          )}
        </div>
      </div>
      <div className="relative flex flex-wrap gap-[10px]">
        <button type="button" onClick={() => void openExternal(PEEBIFY_DISCORD_URL)} className={LINK_BUTTON}>
          <Mark path={BRAND_PATHS.discord} size={16} />
          {t("settings.about.discord")}
        </button>
        <button type="button" onClick={() => void openExternal(PEEBIFY_SITE_URL)} className={LINK_BUTTON}>
          <Globe size={15} />
          {t("settings.about.website")}
        </button>
      </div>
    </section>
  );
}

export function AboutCredits() {
  const t = useT();
  return (
    <>
      <Section label={t("settings.about.developer")}>
        <PersonCard person={DEVELOPER} />
      </Section>

      <Section label={t("settings.about.contributors")}>
        <div className="grid grid-cols-2 gap-3 max-[760px]:grid-cols-1">
          {CONTRIBUTORS.map((p) => (
            <PersonCard key={p.github} person={p} />
          ))}
        </div>
      </Section>

      <Section
        label={t("settings.about.xxmiMakers")}
        aside={
          <button
            type="button"
            onClick={() => void openExternal(XXMI_URL)}
            className="inline-flex items-center gap-[5px] text-[12.5px] font-medium text-white/70 transition-colors hover:text-white"
          >
            {t("settings.about.xxmiLink")}
            <ArrowUpRight size={12} />
          </button>
        }
      >
        <p className="mb-3 px-1 text-[12px] leading-snug text-white/55">{t("settings.about.xxmiDescription")}</p>
        <div className="grid grid-cols-2 gap-3 max-[760px]:grid-cols-1">
          {XXMI_MAKERS.map((p) => (
            <PersonCard key={p.github} person={p} />
          ))}
        </div>
      </Section>
    </>
  );
}
