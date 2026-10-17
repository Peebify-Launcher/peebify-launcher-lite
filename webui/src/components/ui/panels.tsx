// ------------ Panels ------------
// The building blocks of settings-style pages, shared with the overlay: the progress bar, the setting card, and the
// boxes and headings that group them.
import type { ReactNode } from "react";

// ------------ Progress Bar ------------
// The plain progress bar used for mod installs and similar. Always shows a sliver once something has started so
// it never looks empty. The overlay's compact bar is plain white on a faint track.
const HEIGHTS = { sm: "h-[4px]", md: "h-[6px]" } as const;

export function ProgressBar({
  value,
  size = "md",
  ariaLabel,
  compact = false,
  className = "",
}: {
  value: number;
  size?: keyof typeof HEIGHTS;
  ariaLabel?: string;
  compact?: boolean;
  className?: string;
}) {
  const width = value <= 0 ? 0 : Math.min(Math.max(value, compact ? 0 : 1.5), 100);
  return (
    <div
      role="progressbar"
      aria-label={ariaLabel}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={Math.round(value)}
      className={`relative ${HEIGHTS[size]} overflow-hidden ${
        compact ? "rounded-[3px] bg-white/[0.08]" : "progress-track rounded-full"
      } ${className}`}
    >
      <div
        className={`absolute inset-y-0 left-0 overflow-hidden ${
          compact
            ? "rounded-[3px] bg-white/85"
            : "progress-fill rounded-full transition-[width] duration-150 ease-linear"
        }`}
        style={{ width: `${width}%` }}
      />
    </div>
  );
}

// ------------ Setting Card ------------
// One row in a settings box: an icon, a title and short description on the left and the control (switch, button,
// dropdown) on the right. Can be dimmed, red for dangerous actions, or carry a badge. The overlay uses the smaller
// compact row.
interface SettingCardProps {
  title: string;
  description?: string;
  control?: ReactNode;
  icon?: ReactNode;
  disabled?: boolean;
  danger?: boolean;
  badge?: ReactNode;
  detail?: ReactNode;
  nested?: boolean;
  compact?: boolean;
}

export function SettingCard({
  title,
  description,
  control,
  icon,
  disabled = false,
  danger = false,
  badge,
  detail,
  nested = false,
  compact = false,
}: SettingCardProps) {
  const box = compact
    ? "px-4 py-[13px]"
    : `py-4 pr-5 transition-colors duration-150 ${nested ? "bg-black/[0.12] pl-[68px]" : "pl-5"}`;
  return (
    <div
      inert={disabled}
      className={`flex items-center justify-between gap-4 ${box} ${disabled ? "pointer-events-none opacity-50" : ""}`}
    >
      <div className="flex min-w-0 items-center gap-3">
        {icon && !nested && (
          <span
            className="flex h-9 w-9 shrink-0 items-center justify-center rounded-[8px] bg-white/[0.05]"
            style={{ color: danger ? "#fca5a5" : "rgba(255,255,255,0.7)" }}
          >
            {icon}
          </span>
        )}
        <div className="min-w-0 text-left">
          <div className="flex items-center gap-2">
            <h5
              className={`${compact ? "text-[12.5px]" : "text-[14px]"} font-medium`}
              style={{ color: danger ? "#fca5a5" : "#fff" }}
            >
              {title}
            </h5>
            {badge}
          </div>
          {description && (
            <p
              className={`max-w-[56ch] text-white/55 ${
                compact ? "mt-[3px] text-[11.5px] leading-[1.45]" : "mt-1 text-[12.5px] leading-snug wrap-anywhere"
              }`}
            >
              {description}
            </p>
          )}
          {detail}
        </div>
      </div>
      <div className="flex shrink-0 items-center gap-2">{control}</div>
    </div>
  );
}

// ------------ Settings Group ------------
// The boxes and headings that group settings together, including the red-tinted box for dangerous ones. The overlay
// uses the compact ones: a slightly squarer, fainter box and a smaller heading.
export function GroupBox({
  children,
  danger = false,
  divided = false,
  compact = false,
  className = "",
}: {
  children: ReactNode;
  danger?: boolean;
  divided?: boolean;
  compact?: boolean;
  className?: string;
}) {
  const tone = danger
    ? "border-[#ef4444]/[0.22] bg-[#ef4444]/[0.03]"
    : compact
      ? "border-white/[0.08] bg-white/[0.045]"
      : "border-white/[0.08] bg-white/[0.05]";
  const lines = compact ? "divide-y divide-white/[0.055] " : "divide-y divide-white/[0.06] ";
  return (
    <div
      className={`overflow-hidden border ${compact ? "rounded-[10px]" : "rounded-ui"} ${divided ? lines : ""}${tone} ${className}`}
    >
      {children}
    </div>
  );
}

export function GroupLabel({
  label,
  hint,
  compact = false,
  className = "",
}: {
  label: string;
  hint?: string;
  compact?: boolean;
  className?: string;
}) {
  if (compact) {
    return <div className="text-[11px] uppercase tracking-[0.08em] text-white/55">{label}</div>;
  }
  return (
    <div className={`mb-[10px] flex items-center justify-between gap-3 px-1 ${className}`}>
      <h3 className="text-[12px] font-semibold uppercase tracking-[0.07em] text-white/55">
        {label}
        {hint && (
          <span className="ml-2 font-normal normal-case tracking-normal text-white/55">{hint}</span>
        )}
      </h3>
    </div>
  );
}

export function SettingsGroup({
  label,
  hint,
  danger = false,
  children,
}: {
  label?: string;
  hint?: string;
  danger?: boolean;
  children: ReactNode;
}) {
  return (
    <section>
      {label && <GroupLabel label={label} hint={hint} />}
      <GroupBox danger={danger}>{children}</GroupBox>
    </section>
  );
}
