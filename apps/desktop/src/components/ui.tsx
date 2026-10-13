/**
 * Small iOS-style building blocks: grouped sections, rows, switches,
 * segmented controls, status dots, banners. Pure presentation — no state
 * beyond what the caller passes in.
 */
import type { ReactNode } from "react";

export function Section({
  title,
  footer,
  children,
  action,
}: {
  title?: string;
  footer?: ReactNode;
  children: ReactNode;
  action?: ReactNode;
}) {
  return (
    <section className="section">
      {(title || action) && (
        <div className="section-head">
          {title && <h3 className="section-label">{title}</h3>}
          {action}
        </div>
      )}
      <div className="group">{children}</div>
      {footer && <div className="section-footer">{footer}</div>}
    </section>
  );
}

export function Row({
  icon,
  title,
  subtitle,
  trailing,
  onClick,
  selected,
  dim,
}: {
  icon?: ReactNode;
  title: string;
  subtitle?: string | null;
  trailing?: ReactNode;
  onClick?: () => void;
  selected?: boolean;
  dim?: boolean;
}) {
  const cls = "row" + (onClick ? " tappable" : "") + (selected ? " selected" : "") + (dim ? " dim" : "");
  return (
    <div
      className={cls}
      onClick={onClick}
      role={onClick ? "button" : undefined}
      tabIndex={onClick ? 0 : undefined}
      onKeyDown={(e) => {
        if (onClick && (e.key === "Enter" || e.key === " ")) {
          e.preventDefault();
          onClick();
        }
      }}
    >
      {icon && <span className="row-icon">{icon}</span>}
      <span className="row-text">
        <span className="row-title" title={title}>
          {title}
        </span>
        {subtitle && (
          <span className="row-subtitle" title={subtitle}>
            {subtitle}
          </span>
        )}
      </span>
      {trailing && (
        <span className="row-trailing" onClick={(e) => e.stopPropagation()}>
          {trailing}
        </span>
      )}
    </div>
  );
}

export function Switch({
  checked,
  onChange,
  label,
  disabled,
}: {
  checked: boolean;
  onChange: (v: boolean) => void;
  label: string;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      role="switch"
      aria-checked={checked}
      aria-label={label}
      title={label}
      disabled={disabled}
      className={"switch" + (checked ? " on" : "")}
      onClick={() => onChange(!checked)}
    >
      <span className="switch-knob" />
    </button>
  );
}

export function Segmented<T extends string | number>({
  value,
  options,
  onChange,
  label,
  size,
}: {
  value: T;
  options: { value: T; label: string; title?: string }[];
  onChange: (v: T) => void;
  label: string;
  size?: "small";
}) {
  return (
    <div className={"segmented" + (size === "small" ? " small" : "")} role="radiogroup" aria-label={label}>
      {options.map((o) => (
        <button
          key={String(o.value)}
          type="button"
          role="radio"
          aria-checked={o.value === value}
          className={o.value === value ? "active" : ""}
          title={o.title ?? o.label}
          onClick={() => onChange(o.value)}
        >
          {o.label}
        </button>
      ))}
    </div>
  );
}

export type Tone = "ok" | "warn" | "bad" | "off" | "live";

export function Dot({ tone }: { tone: Tone }) {
  return <span className={`dot ${tone}`} aria-hidden="true" />;
}

export function Banner({
  tone,
  children,
  onClose,
}: {
  tone: "error" | "info" | "warn";
  children: ReactNode;
  onClose?: () => void;
}) {
  return (
    <div className={`banner ${tone}`} role={tone === "error" ? "alert" : "status"}>
      <div className="banner-text">{children}</div>
      {onClose && (
        <button type="button" className="banner-close" onClick={onClose} aria-label="Dismiss">
          ×
        </button>
      )}
    </div>
  );
}

export function Empty({ title, hint }: { title: string; hint?: string }) {
  return (
    <div className="empty">
      <div className="empty-title">{title}</div>
      {hint && <div className="empty-hint">{hint}</div>}
    </div>
  );
}

/** Labelled control on one line (label left, control right). */
export function Field({ label, children, hint }: { label: string; children: ReactNode; hint?: string }) {
  return (
    <label className="field">
      <span className="field-label">{label}</span>
      <span className="field-control">{children}</span>
      {hint && <span className="field-hint">{hint}</span>}
    </label>
  );
}

/** Simple monochrome glyphs (no emoji, no icon font). */
export function Glyph({ name }: { name: "camera" | "screen" | "media" | "mic" | "text" | "color" | "pattern" | "capture" }) {
  const p: Record<string, string> = {
    camera: "M4 7h3l2-2h6l2 2h3v12H4z M12 10a3 3 0 1 0 0 6 3 3 0 0 0 0-6",
    capture: "M3 6h18v10H3z M8 20h8 M12 16v4",
    screen: "M3 5h18v11H3z M9 20h6 M12 16v4",
    media: "M5 4l14 8-14 8z",
    mic: "M12 3a3 3 0 0 0-3 3v6a3 3 0 0 0 6 0V6a3 3 0 0 0-3-3 M6 11a6 6 0 0 0 12 0 M12 17v4",
    text: "M5 6h14 M12 6v13",
    color: "M12 4a8 8 0 1 0 0 16 8 8 0 0 0 0-16",
    pattern: "M4 4h7v7H4z M13 13h7v7h-7z",
  };
  return (
    <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
      <path d={p[name]} />
    </svg>
  );
}
