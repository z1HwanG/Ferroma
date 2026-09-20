/**
 * What the two stand-alone pages of a first install share.
 *
 * Before there is an administrator the console has no sidebar and no top bar, and the page
 * that does the work carries its own header: the mark, a title, a line of copy, and the two
 * controls an operator needs before they have an account — the interface language and
 * light/dark. The database step and the setup wizard are separate pages with the same
 * furniture, so the furniture lives here.
 */

import { el } from '../../shared/dom.js';
import { LOCALES, currentLocale, setLocale, t } from '../../shared/i18n.js';
import { onThemeChange, resolvedTheme, toggleTheme } from '../../shared/theme.js';
import { icon } from '../icons.js';

/**
 * The language picker and the theme switch.
 *
 * Both apply immediately: there is no session to save a preference against yet, and the
 * page a first-time reader is looking at has to be readable *before* they can sign in.
 *
 * @returns {Element}
 */
export function firstRunTools() {
  const language = el('div', { class: 'segmented', role: 'radiogroup', 'aria-label': t('Language') });
  for (const entry of LOCALES) {
    const button = el('button', {
      type: 'button',
      class: 'segmented-option',
      role: 'radio',
      'aria-checked': entry.tag === currentLocale() ? 'true' : 'false',
      text: entry.label,
    });
    button.addEventListener('click', () => {
      if (entry.tag === currentLocale()) return;
      setLocale(entry.tag);
      window.location.reload();
    });
    language.append(button);
  }

  const theme = el(
    'button',
    {
      type: 'button',
      class: 'btn btn-icon theme-toggle',
      id: 'first-run-theme-toggle',
      'aria-pressed': resolvedTheme() === 'dark' ? 'true' : 'false',
      'aria-label': t('Switch theme'),
      title: t('Switch light / dark'),
    },
    [icon('sun', 'icon icon-sun'), icon('moon', 'icon icon-moon')],
  );
  const sync = () => theme.setAttribute('aria-pressed', resolvedTheme() === 'dark' ? 'true' : 'false');
  onThemeChange(sync);
  theme.addEventListener('click', (event) => toggleTheme({ origin: event.currentTarget }));

  return el('div', { class: 'setup-tools' }, [language, theme]);
}

/**
 * The heading of a stand-alone first-run page.
 *
 * @param {string} title already translated
 * @param {string} lede already translated
 * @returns {Element}
 */
export function firstRunHeader(title, lede) {
  return el('header', { class: 'setup-hero' }, [
    el('span', { class: 'brand-mark', 'aria-hidden': 'true' }, [icon('setup', '')]),
    el('div', { class: 'setup-hero-text' }, [
      el('h1', { class: 'setup-title', text: title }),
      el('p', { class: 'setup-lede', text: lede }),
    ]),
    firstRunTools(),
  ]);
}
