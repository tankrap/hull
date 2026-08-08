// keel/hull Tailwind config. Colors read CSS variables (globals.css) so
// [data-theme="dark"] switches the whole palette with no class churn.
/** @type {import('tailwindcss').Config} */
// Wrap CSS-variable colors so Tailwind's `/opacity` modifiers compile: `<alpha-value>` is
// substituted by Tailwind (1 when no modifier, the fraction when e.g. `/30`), and color-mix
// folds it into the var — keeping the whole palette theme-switchable via [data-theme].
const a = (c) => `color-mix(in oklab, ${c} calc(<alpha-value> * 100%), transparent)`;
module.exports = {
  content: ['./index.html', './src/**/*.{js,jsx,ts,tsx}'],
  theme: {
    extend: {
      fontFamily: {
        sans: ['Inter', '"GitLab Sans"', '-apple-system', 'BlinkMacSystemFont', '"Segoe UI"', 'system-ui', 'sans-serif'],
        mono: ['"JetBrains Mono"', '"GitLab Mono"', 'ui-monospace', 'SFMono-Regular', 'Menlo', 'monospace'],
      },
      colors: {
        shell: a('var(--shell)'),
        paper: a('var(--paper)'),
        surface: a('var(--surface)'),
        frame: '#F7F8FA',
        'rest-tile': '#EEF0F3',
        rule: a('var(--rule)'),
        rule2: a('var(--rule2)'),
        rule3: a('var(--rule3)'),
        ctl: a('var(--ctl-border)'),
        ctlfill: { DEFAULT: a('var(--ctl-fill)'), hover: a('var(--ctl-fill-hover)') },
        ink: a('var(--ink)'),
        body: a('var(--body)'),
        dim: a('var(--dim)'),
        muted: a('var(--muted)'),
        faint: a('var(--faint)'),
        steel: { DEFAULT: a('var(--steel)'), text: a('var(--steel-text)'), wash: a('var(--steel-wash)') },
        brass: { DEFAULT: a('var(--brass)'), text: a('var(--brass-text)'), wash: a('var(--brass-wash)') },
        clear: { DEFAULT: a('var(--clear)'), text: a('var(--clear-text)'), wash: a('var(--clear-wash)') },
        fault: { DEFAULT: a('var(--fault)'), text: a('var(--fault-text)'), wash: a('var(--fault-wash)') },
        ramp: { 1: '#2563EB', 2: '#4C7BEF', 3: '#6E96F3', 4: '#93B4F6', 5: '#C7D8FA' },
      },
      borderRadius: {
        // GitLab/Pajamas radii: 4px default control, 8px for cards/panels — crisp, not pillowy.
        card: '8px',    // cards, alerts, modals
        ctl: '4px',     // controls (buttons / inputs) — GitLab's crisp 4px
        'ctl-sm': '3px',
        chip: '4px',    // selection chips
        badge: '4px',   // lozenge radius
        frame: '10px',  // chart frames / content-well corner
      },
      height: {
        ctl: '32px', 'ctl-sm': '26px', 'ctl-lg': '40px',
      },
      width: {
        ctl: '32px', 'ctl-sm': '26px', 'ctl-lg': '40px',
      },
      boxShadow: {
        // Neutral-black shadows read correctly in BOTH themes (a navy-tinted shadow vanishes on the
        // dark shell). Paired with a hairline border, modals/menus sit crisply on either ground.
        menu: '0 10px 28px -10px rgba(0,0,0,0.45)',
        modal: '0 24px 60px -16px rgba(0,0,0,0.6)',
        drawer: '-14px 0 40px -16px rgba(0,0,0,0.5)',
        'chart-card': '0 0 1.76px rgba(0,0,0,0.08), 0 1px 1.76px rgba(25,28,33,0.06), 0 0 0 1px rgba(25,28,33,0.04)',
      },
      transitionTimingFunction: {
        out: 'cubic-bezier(0.19, 1, 0.22, 1)',
      },
      keyframes: {
        'bd-in': { from: { opacity: 0 }, to: { opacity: 1 } },
        'ov-in': { from: { opacity: 0, transform: 'translate(-50%,-48%) scale(0.96)' }, to: { opacity: 1, transform: 'translate(-50%,-50%) scale(1)' } },
        'dw-in': { from: { transform: 'translateX(100%)' }, to: { transform: 'translateX(0)' } },
      },
      animation: {
        'bd-in': 'bd-in 150ms ease-out',
        'ov-in': 'ov-in 180ms cubic-bezier(0.19,1,0.22,1)',
        'dw-in': 'dw-in 220ms cubic-bezier(0.19,1,0.22,1)',
      },
    },
  },
  plugins: [],
};
