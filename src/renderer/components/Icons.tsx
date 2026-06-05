// Small monochrome SVG icons for the icon-only top bar. They inherit color via
// `currentColor` and size via CSS (width/height: 1em on the button), so the bar
// stays text-free — every control is an icon with a `title` tooltip.
import type { SVGProps } from 'react';

type IconProps = SVGProps<SVGSVGElement>;

function Svg({ children, ...props }: IconProps) {
  return (
    <svg
      viewBox="0 0 24 24"
      width="1em"
      height="1em"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.7}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
      {...props}
    >
      {children}
    </svg>
  );
}

/** The hyperpanes mark: a main + stack tile layout, drawn in the brand gradient. */
export function LogoMark(props: IconProps) {
  return (
    <svg viewBox="0 0 24 24" width="1em" height="1em" fill="none" aria-hidden="true" {...props}>
      <defs>
        <linearGradient id="hp-grad" x1="2" y1="3" x2="22" y2="21" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="#4ade80" />
          <stop offset="1" stopColor="#22d3ee" />
        </linearGradient>
      </defs>
      <g stroke="url(#hp-grad)" strokeWidth={1.8} strokeLinejoin="round">
        <rect x="2.5" y="4" width="19" height="16" rx="2.5" />
        <line x1="11" y1="4" x2="11" y2="20" />
        <line x1="11" y1="12" x2="21.5" y2="12" />
      </g>
    </svg>
  );
}

/** Hamburger / application menu. */
export const IconMenu = (p: IconProps) => (
  <Svg {...p}>
    <line x1="4" y1="7" x2="20" y2="7" />
    <line x1="4" y1="12" x2="20" y2="12" />
    <line x1="4" y1="17" x2="20" y2="17" />
  </Svg>
);

/** Settings gear — used for the Preferences menu entry. */
export const IconSettings = (p: IconProps) => (
  <Svg {...p}>
    <circle cx="12" cy="12" r="3" />
    <path d="M12 3.5v2.2M12 18.3v2.2M5.2 5.2l1.6 1.6M17.2 17.2l1.6 1.6M3.5 12h2.2M18.3 12h2.2M5.2 18.8l1.6-1.6M17.2 6.8l1.6-1.6" />
  </Svg>
);

export const IconOpen = (p: IconProps) => (
  <Svg {...p}>
    <path d="M4 7a1.5 1.5 0 0 1 1.5-1.5H9l2 2h6.5A1.5 1.5 0 0 1 19 9v8.5A1.5 1.5 0 0 1 17.5 19h-12A1.5 1.5 0 0 1 4 17.5Z" />
  </Svg>
);

export const IconSave = (p: IconProps) => (
  <Svg {...p}>
    <path d="M5 5.5h10.5L19 9v9.5a1 1 0 0 1-1 1H6a1 1 0 0 1-1-1Z" />
    <path d="M8 5.5v4h6v-4" />
    <rect x="8" y="12.5" width="8" height="5" rx="0.5" />
  </Svg>
);

/** Command palette — a terminal prompt glyph, echoing the brand. */
export const IconCommands = (p: IconProps) => (
  <Svg {...p}>
    <rect x="3.5" y="5" width="17" height="14" rx="2" />
    <path d="M7.5 10l2.5 2-2.5 2" />
    <line x1="12.5" y1="15" x2="16" y2="15" />
  </Svg>
);

/** Framed "+" — a tile with a plus. Reads as "add a pane" (used for New pane). */
export const IconPlus = (p: IconProps) => (
  <Svg {...p}>
    <rect x="4" y="4" width="16" height="16" rx="2.5" />
    <line x1="12" y1="8.5" x2="12" y2="15.5" />
    <line x1="8.5" y1="12" x2="15.5" y2="12" />
  </Svg>
);

/** Bare "+" — the universal new-tab glyph (no surrounding tile). */
export const IconPlusBare = (p: IconProps) => (
  <Svg {...p}>
    <line x1="12" y1="6" x2="12" y2="18" />
    <line x1="6" y1="12" x2="18" y2="12" />
  </Svg>
);


// ---- Window controls ----
export const IconMinimize = (p: IconProps) => (
  <Svg {...p}>
    <line x1="6" y1="16" x2="18" y2="16" />
  </Svg>
);

export const IconMaximize = (p: IconProps) => (
  <Svg {...p}>
    <rect x="6" y="6" width="12" height="12" rx="1.5" />
  </Svg>
);

export const IconRestore = (p: IconProps) => (
  <Svg {...p}>
    <path d="M9 9V7.5A1.5 1.5 0 0 1 10.5 6h6A1.5 1.5 0 0 1 18 7.5v6A1.5 1.5 0 0 1 16.5 15H15" />
    <rect x="6" y="9" width="9" height="9" rx="1.5" />
  </Svg>
);

export const IconClose = (p: IconProps) => (
  <Svg {...p}>
    <line x1="6.5" y1="6.5" x2="17.5" y2="17.5" />
    <line x1="17.5" y1="6.5" x2="6.5" y2="17.5" />
  </Svg>
);
