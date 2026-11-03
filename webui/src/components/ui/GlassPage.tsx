// ------------ Glass Page ------------
// The common frame for a full page: a title and subtitle at the top, optional controls on the right, and the
// page content scrolling below.
import type { ReactNode } from "react";

export function GlassPage({
  title,
  subtitle,
  headerRight,
  children,
}: {
  title: string;
  subtitle?: string;
  headerRight?: ReactNode;
  children: ReactNode;
}) {
  return (
    <div className="absolute inset-0 overflow-y-auto overscroll-contain px-10 py-8">
      <header className="mb-7 flex flex-wrap items-end justify-between gap-5 border-b border-white/10 pb-4">
        <div>
          <h1 className="text-[24px] font-semibold leading-tight">{title}</h1>
          {subtitle && <p className="mt-1 text-[14px] text-white/50">{subtitle}</p>}
        </div>
        {headerRight}
      </header>
      {children}
    </div>
  );
}
