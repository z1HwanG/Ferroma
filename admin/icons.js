/**
 * Navigation and toolbar icons for the Admin console.
 *
 * Every icon is a stroked 24×24 path list, matching the inline SVGs in
 * `index.html`, so the sidebar, the topbar and the buttons share one visual
 * weight. Like `dom.js#svgIcon`, the nodes are built through
 * `createElementNS` and never parse markup — nothing here can be influenced by
 * server data.
 */

/** @type {Record<string, string[]>} */
const PATHS = {
  dashboard: ['M4 4h7v7H4z', 'M13 4h7v4h-7z', 'M13 10h7v10h-7z', 'M4 13h7v7H4z'],
  domains: ['M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18Z', 'M3.5 9h17M3.5 15h17', 'M12 3c2.4 2.4 3.6 5.4 3.6 9s-1.2 6.6-3.6 9c-2.4-2.4-3.6-5.4-3.6-9S9.6 5.4 12 3Z'],
  users: ['M12 11a3.5 3.5 0 1 0 0-7 3.5 3.5 0 0 0 0 7Z', 'M4.5 20c0-3.4 3.4-5.4 7.5-5.4s7.5 2 7.5 5.4'],
  aliases: ['M16.5 12a4.5 4.5 0 1 1-1.3-3.2', 'M16.5 8v5a2 2 0 0 0 3.4 1.4A8.5 8.5 0 1 0 12 20.5'],
  queue: ['M4 7h16', 'M4 12h16', 'M4 17h16', 'M7 4v6M17 15v6'],
  logs: ['M5 4h11l3 3v13H5z', 'M8 9h8M8 13h8M8 17h5'],
  storage: ['M4 6c0-1.1 3.6-2 8-2s8 .9 8 2-3.6 2-8 2-8-.9-8-2Z', 'M4 6v12c0 1.1 3.6 2 8 2s8-.9 8-2V6', 'M4 12c0 1.1 3.6 2 8 2s8-.9 8-2'],
  devices: ['M3 5h13v9H3z', 'M7 18h6', 'M9.5 14v4', 'M18 9h3v9h-3z'],
  tls: ['M12 3 5 6v6c0 4 3 7.2 7 9 4-1.8 7-5 7-9V6l-7-3Z', 'M9.5 12.5 11.5 15l3.5-4.5'],
  audit: ['M6 3h9l3 3v15H6z', 'M9 12.5 11 15l4-5', 'M9 8h6'],
  // Sliders rather than a gear: at 16–18px a ring with eight spokes reads as a sun,
  // and this icon sits beside a theme toggle in both front-ends.
  settings: [
    'M4 7h3M11 7h9',
    'M4 12h9M17 12h3',
    'M4 17h2M10 17h10',
    'M9 5a2 2 0 1 0 0 4 2 2 0 0 0 0-4Z',
    'M15 10a2 2 0 1 0 0 4 2 2 0 0 0 0-4Z',
    'M8 15a2 2 0 1 0 0 4 2 2 0 0 0 0-4Z',
  ],
  sun: ['M12 16a4 4 0 1 0 0-8 4 4 0 0 0 0 8Z', 'M12 2v2M12 20v2M2 12h2M20 12h2M5 5l1.4 1.4M17.6 17.6 19 19M19 5l-1.4 1.4M6.4 17.6 5 19'],
  moon: ['M20 14.5A8.5 8.5 0 0 1 9.5 4a8.5 8.5 0 1 0 10.5 10.5Z'],
  setup: ['M4 6h10M18 6h2', 'M4 12h4M12 12h8', 'M4 18h12M20 18h0', 'M14 4v4M8 10v4M16 16v4'],
  search: ['M11 17a6 6 0 1 0 0-12 6 6 0 0 0 0 12Z', 'M15.5 15.5 21 21'],
  refresh: ['M20 11a8 8 0 1 0-1.2 5.2', 'M20 5v6h-6'],
  plus: ['M12 5v14M5 12h14'],
  close: ['M6 6l12 12M18 6 6 18'],
  chevron: ['m9 6 6 6-6 6'],
  chevronDown: ['m6 9 6 6 6-6'],
  external: ['M14 4h6v6', 'M20 4l-8 8', 'M18 14v6H4V6h6'],
  /* Row actions and stat tiles. A tile with a glyph is read at a glance, and a row
     action with one is found without reading five labels in a row. */
  details: ['M12 5c5 0 8.5 4 9.5 7-1 3-4.5 7-9.5 7S3.5 15 2.5 12C3.5 9 7 5 12 5Z', 'M12 15a3 3 0 1 0 0-6 3 3 0 0 0 0 6Z'],
  edit: ['M4 20h4l10-10-4-4L4 16v4Z', 'm14 6 4 4'],
  key: ['M15 4a5 5 0 1 0-1.6 9.7L11 16H8v3H5l-1 1v-3l8-8.6A5 5 0 0 1 15 4Z', 'M16.5 7.5h.01'],
  power: ['M12 3v9', 'M7 7a7 7 0 1 0 10 0'],
  trash: ['M5 7h14M9 7V4h6v3M7 7l1 14h8l1-14'],
  check: ['m5 12.5 4.5 4.5L19 7'],
  copy: ['M9 9h10v11H9z', 'M5 15V4h10'],
  download: ['M12 4v10', 'm8 11 4 4 4-4', 'M5 19h14'],
  warning: ['M12 4 3 20h18L12 4Z', 'M12 10v4M12 17h.01'],
  mail: ['M3 6h18v12H3z', 'm3 7 9 6 9-6'],
  server: ['M4 5h16v6H4z', 'M4 13h16v6H4z', 'M7.5 8h.01M7.5 16h.01'],
  database: ['M4 6c0-1.1 3.6-2 8-2s8 .9 8 2-3.6 2-8 2-8-.9-8-2Z', 'M4 6v12c0 1.1 3.6 2 8 2s8-.9 8-2V6', 'M4 12c0 1.1 3.6 2 8 2s8-.9 8-2'],
  activity: ['M3 12h4l2.5-6 4 12 2.5-6h5'],
  clock: ['M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18Z', 'M12 7v5l3 2'],
  filter: ['M3 5h18l-7 8v6l-4-2v-4L3 5Z'],
  inbox: ['M3 12h5l2 3h4l2-3h5', 'M3 12 5 5h14l2 7v6a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1v-6Z'],
  sent: ['m4 12 16-8-6 16-2-6-8-2Z'],
  clip: ['M8.5 12.5 14 7a3 3 0 0 1 4 4l-7.5 7.5a5 5 0 0 1-7-7L11 4'],
};

/**
 * Build one icon.
 * @param {string} name a key of the path table; unknown names fall back to a dot
 * @param {string} [className]
 */
export function icon(name, className = 'icon') {
  const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
  svg.setAttribute('viewBox', '0 0 24 24');
  svg.setAttribute('aria-hidden', 'true');
  svg.setAttribute('focusable', 'false');
  svg.setAttribute('class', className);
  for (const d of PATHS[name] || PATHS.dashboard) {
    const path = document.createElementNS('http://www.w3.org/2000/svg', 'path');
    path.setAttribute('d', d);
    path.setAttribute('fill', 'none');
    path.setAttribute('stroke', 'currentColor');
    path.setAttribute('stroke-width', '1.8');
    path.setAttribute('stroke-linecap', 'round');
    path.setAttribute('stroke-linejoin', 'round');
    svg.append(path);
  }
  return svg;
}
