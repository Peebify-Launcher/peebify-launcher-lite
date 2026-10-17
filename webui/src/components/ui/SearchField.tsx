// ------------ Search Field ------------
// A search box with a magnifier and a clear button. Escape clears what you typed.
import { Search, X } from "lucide-react";
import { useT } from "../../lib/useT";

export function SearchField({
  value,
  onChange,
  placeholder,
  ariaLabel,
  className = "min-w-0 flex-1",
}: {
  value: string;
  onChange: (value: string) => void;
  placeholder: string;
  ariaLabel: string;
  className?: string;
}) {
  const t = useT();
  return (
    <div className={`relative ${className}`}>
      <Search
        size={15}
        className="pointer-events-none absolute left-3 top-1/2 -translate-y-1/2 text-white/35"
      />
      <input
        type="text"
        value={value}
        spellCheck={false}
        placeholder={placeholder}
        aria-label={ariaLabel}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Escape" && value) {
            e.preventDefault();
            e.stopPropagation();
            onChange("");
          }
        }}
        className="w-full rounded-ui border border-white/[0.08] bg-black/30 py-[9px] pl-9 pr-9 text-[13px] text-white/85 placeholder:text-white/30 focus:border-(--accent-a)"
      />
      {value && (
        <button
          type="button"
          onClick={() => onChange("")}
          aria-label={t("ui.searchField.clear")}
          className="absolute right-2 top-1/2 -translate-y-1/2 rounded-[6px] p-1 text-white/40 transition hover:bg-white/[0.08] hover:text-white"
        >
          <X size={14} />
        </button>
      )}
    </div>
  );
}
