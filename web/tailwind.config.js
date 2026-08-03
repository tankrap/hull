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
        sans: ['"Mona Sans"', '"Helvetica Neue"', 'Helvetica', 'system-ui', 'sans-serif'],
      },
      colors: {
        paper: a('var(--paper)'),
        surface: a('var(--surface)'),
        frame: '#F7F8FA',
        'rest-tile': '#EEF0F3',
        rule: a('var(--rule)'),
        rule2: a('var(--rule2)'),
        rule3: a('var(--rule3)'),
        ctl: a('var(--ctl-border)'),
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
        card: '8px',    // cards, alerts — ADS border.radius.200
        ctl: '6px',     // controls — ADS border.radius.100/150
        'ctl-sm': '5px',
        chip: '4px',    // selection chips
        badge: '3px',   // ADS lozenge radius
        frame: '12px',  // chart frames
      },
      height: {
        ctl: '32px', 'ctl-sm': '26px', 'ctl-lg': '40px',
      },
      width: {
        ctl: '32px', 'ctl-sm': '26px', 'ctl-lg': '40px',
      },
      boxShadow: {
        menu: '0 8px 24px -8px rgba(15,23,42,0.25)',
        modal: '0 16px 40px -12px rgba(15,23,42,0.4)',
        drawer: '-12px 0 32px -12px rgba(15,23,42,0.3)',
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
