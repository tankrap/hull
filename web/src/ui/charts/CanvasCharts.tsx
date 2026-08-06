// keel/hull canvas chart engines — Donut, StackedBars, AreaField, LineChart.
// These four are canvas because the drifting-tile fills and springs can't be DOM.
// Each is a React component owning one <canvas> + a rAF loop. Tailwind handles the
// frame (use ChartCard/ChartFrame); this file is the drawing logic, ported 1:1 from
// the reference in "Keel Brand Guidelines.dc.html" section 10.
import { useEffect, useRef } from 'react';
import { RAMP } from './ChartCard';

// ---- shared helpers --------------------------------------------------------
const hash = (x: number, y: number) => { const v = Math.sin(x * 127.1 + y * 311.7) * 43758.5453; return v - Math.floor(v); };
const ss = (a: number, b: number, x: number) => { x = Math.min(1, Math.max(0, (x - a) / (b - a))); return x * x * (3 - 2 * x); };
const reduced = () => matchMedia('(prefers-reduced-motion: reduce)').matches;

type DrawFn = (ctx: CanvasRenderingContext2D, w: number, h: number, t: number) => void;

function useCanvasLoop(draw: DrawFn) {
  const ref = useRef<HTMLCanvasElement | null>(null);
  useEffect(() => {
    let raf = 0, t = 0;
    const loop = () => {
      raf = requestAnimationFrame(loop);
      if (!reduced()) t += 0.02;
      const cv = ref.current; if (!cv) return;
      const w = cv.clientWidth, h = cv.clientHeight; if (!w) return;
      const dpr = Math.min(2, devicePixelRatio || 1);
      if (cv.width !== Math.ceil(w * dpr)) { cv.width = Math.ceil(w * dpr); cv.height = Math.ceil(h * dpr); }
      const ctx = cv.getContext('2d')!;
      ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
      ctx.clearRect(0, 0, w, h);
      draw(ctx, w, h, t);
    };
    loop();
    return () => cancelAnimationFrame(raf);
  }, [draw]);
  return ref;
}

// ---- Donut (voyages by model) ----------------------------------------------
// shares: number[] summing to 1 · hover: index|null
// Tile-field ring: inner r 52, outer 89 of a 200 unit box. Hovered segment lifts
// 6px along its mid-angle over a 12% wash; others dim to 0.216 (0.72 * 0.3).
export function Donut({ shares, hover = null }: { shares: number[]; hover?: number | null }) {
  const draw: DrawFn = (ctx, w, h, t) => {
    const s = Math.min(w, h) / 200;
    ctx.setTransform(ctx.getTransform().a * s, 0, 0, ctx.getTransform().d * s, 0, 0);
    const cell = 4.6, any = hover != null;
    let cum = -Math.PI / 2;
    const angles = shares.map((sh) => { const sw = sh * Math.PI * 2; const a = { a0: cum + 0.035, a1: cum + sw - 0.035, mid: cum + sw / 2 }; cum += sw; return a; });
    const paths = shares.map(() => new Path2D());
    for (let x = cell / 2; x < 200; x += cell) for (let y = cell / 2; y < 200; y += cell) {
      const dx = x - 100, dy = y - 100, d = Math.hypot(dx, dy);
      if (d < 52.5 || d > 88.5) continue;
      let rel = (Math.atan2(dy, dx) + Math.PI / 2 + Math.PI * 4) % (Math.PI * 2), acc = 0, si = -1;
      for (let i = 0; i < shares.length; i++) { const sw = shares[i] * Math.PI * 2; if (rel >= acc + 0.035 && rel <= acc + sw - 0.035) { si = i; break; } acc += sw; }
      if (si < 0) continue;
      const f = 0.62 + 0.38 * ss(55, 86, d);
      const wv = ss(0, 1, 0.5 + 0.5 * ((Math.sin(x * 0.11 + t) + Math.sin(y * 0.13 + t * 0.7 + 2) + Math.sin((x + y) * 0.07 + t * 1.3 + 4)) / 3));
      const sz = cell * ((hover === si ? 0.46 : 0.34) + 0.36 * f + 0.26 * wv) * (0.78 + 0.42 * hash(x, y));
      paths[si].rect(x - sz / 2, y - sz / 2, sz, sz);
    }
    angles.forEach((a, i) => {
      ctx.save();
      if (hover === i) {
        ctx.translate(Math.cos(a.mid) * 6, Math.sin(a.mid) * 6);
        const u = new Path2D(); u.arc(100, 100, 86, a.a0, a.a1); u.arc(100, 100, 55, a.a1, a.a0, true); u.closePath();
        ctx.globalAlpha = 0.12; ctx.fillStyle = RAMP[i]; ctx.fill(u); ctx.globalAlpha = 1;
      } else ctx.globalAlpha = any ? 0.216 : 0.72;
      ctx.fillStyle = RAMP[i]; ctx.fill(paths[i]); ctx.restore();
    });
  };
  const ref = useCanvasLoop(draw);
  return <canvas ref={ref} className="w-full aspect-square block touch-none" />;
}

// ---- StackedBars (contributions) --------------------------------------------
// cols: number[branches][bands] as 0-1 heights · hover: {c, b}|null
// Tile-filled stacks, 4px band gap, rounded outer corners (top band 8/8/5/5,
// bottom 5/5/7/7). Hovered band: full ramp-1, stroked outline + 10px glow.
export function StackedBars({ cols, hover = null }: { cols: number[][]; hover?: { c: number; b: number } | null }) {
  const draw: DrawFn = (ctx, w, h, t) => {
    const tile = Math.max(3, Math.round(w / 200));
    cols.forEach((bands, r) => {
      const colW = w / cols.length, cx0 = r * colW + colW / 2, bw = Math.min(colW * 0.62, 54);
      let y = h;
      bands.forEach((val, b) => {
        const hgt = Math.max(6, val * (h - 8)), y0 = y - hgt;
        const radii = b === 0 ? [5, 5, 7, 7] : b === bands.length - 1 ? [8, 8, 5, 5] : [5, 5, 5, 5];
        const path = new Path2D(); path.roundRect(cx0 - bw / 2, y0, bw, hgt, radii);
        const bandHov = hover && hover.c === r && hover.b === b;
        const color = bandHov ? RAMP[0] : [RAMP[0], RAMP[2], RAMP[4]][b % 3];
        const alpha = !hover ? 1 : hover.c !== r ? 0.3 : bandHov ? 1 : 0.48;
        ctx.save(); ctx.clip(path);
        const tp = new Path2D();
        for (let tx = cx0 - bw / 2 + tile / 2; tx < cx0 + bw / 2; tx += tile)
          for (let ty = y0 + tile / 2; ty < y; ty += tile) {
            const vf = ss(0, 1, (y - ty) / hgt);
            const fl = 0.5 + 0.5 * ((Math.sin(tx * 0.13 + t) + Math.sin(ty * 0.17 + t * 0.8 + 2) + Math.sin((tx + ty) * 0.06 + t * 1.2 + 4)) / 3);
            const sz = Math.min(tile * 1.35, tile * (0.42 + 0.34 * vf + 0.24 * fl) * (0.8 + 0.4 * hash(tx, ty)) * (bandHov ? 1.5 : 1));
            tp.rect(tx - sz / 2, ty - sz / 2, sz, sz);
          }
        ctx.globalAlpha = alpha; ctx.fillStyle = color; ctx.fill(tp); ctx.restore();
        if (bandHov) { ctx.save(); ctx.strokeStyle = RAMP[0]; ctx.lineWidth = 1.75; ctx.shadowColor = RAMP[0]; ctx.shadowBlur = 10; ctx.stroke(path); ctx.restore(); }
        y = y0 - 4;
      });
    });
  };
  const ref = useCanvasLoop(draw);
  return <canvas ref={ref} className="absolute inset-0 w-full h-full block" />;
}

// ---- AreaField (new voyages) -------------------------------------------------
// series: number[] · max: number. Full tile grid: lit under the curve (soft ramp-4
// below p=0.55, ramp-1 above), rest tiles oklch(0.9 0.02 264) at 26%. Curve top at
// 84% of value/max. Add pointer wash + scrub crosshair per the reference.
export function AreaField({ series, max }: { series: number[]; max: number }) {
  const draw: DrawFn = (ctx, w, h, t) => {
    ctx.imageSmoothingEnabled = false;
    const cell = Math.max(3, Math.round(w / 180)), cols = Math.ceil(w / cell), rows = Math.ceil(h / cell);
    const rest = new Path2D(), rs = cell * 0.26;
    const soft = new Path2D(), strong = new Path2D();
    for (let c = 0; c < cols; c++) {
      const fx = c / (cols - 1), idx = fx * (series.length - 1), i0 = Math.floor(idx), f = idx - i0;
      const v = series[i0] + (series[Math.min(i0 + 1, series.length - 1)] - series[i0]) * (f * f * (3 - 2 * f));
      const topY = h * (1 - 0.84 * v / max);
      for (let r = 0; r < rows; r++) {
        const cy = r * cell + cell / 2, cxp = c * cell + cell / 2;
        if (cy < topY) { rest.rect(cxp - rs / 2, cy - rs / 2, rs, rs); continue; }
        let p = ss(0, 1, Math.max(0, Math.min(1, 1 - (cy - topY) / (h * 0.55))));
        const shimmer = reduced() ? 1 : 1 + 0.07 * Math.sin(t * 2.2 - cy * 0.06);
        const sz = Math.min(cell * 0.98, cell * (0.22 + 0.48 * p) * shimmer);
        (p > 0.55 ? strong : soft).rect(cxp - sz / 2, cy - sz / 2, sz, sz);
      }
    }
    ctx.fillStyle = 'oklch(0.9 0.02 264)'; ctx.fill(rest);
    ctx.fillStyle = RAMP[3]; ctx.fill(soft);
    ctx.fillStyle = RAMP[0]; ctx.fill(strong);
  };
  const ref = useCanvasLoop(draw);
  return <canvas ref={ref} className="absolute inset-0 w-full h-full block" />;
}

// ---- LineChart (latency) ------------------------------------------------------
// series: number[] · domain [min,max]. 2px ramp-1 line, gradient wash to 0,
// 700ms ease-out draw-in, endpoint dot. Scrub readout per the reference.
export function LineChart({ series, domain }: { series: number[]; domain: [number, number] }) {
  const start = useRef(performance.now());
  const draw: DrawFn = (ctx, w, h) => {
    const [lo, hi] = domain, n = series.length;
    const prog = reduced() ? 1 : Math.min(1, (performance.now() - start.current) / 700);
    const e = 1 - Math.pow(1 - prog, 3);
    const X = (i: number) => (i / (n - 1)) * w, Y = (v: number) => (1 - (v - lo) / (hi - lo)) * h;
    const last = Math.max(1, e * (n - 1)), li = Math.floor(last), lf = last - li;
    const path = new Path2D(); path.moveTo(X(0), Y(series[0]));
    for (let i = 1; i <= li; i++) path.lineTo(X(i), Y(series[i]));
    let ex = X(li), ey = Y(series[li]);
    if (li < n - 1 && lf > 0) { ex += (X(li + 1) - X(li)) * lf; ey += (Y(series[li + 1]) - Y(series[li])) * lf; path.lineTo(ex, ey); }
    const area = new Path2D(path); area.lineTo(ex, h); area.lineTo(0, h); area.closePath();
    const g = ctx.createLinearGradient(0, 0, 0, h);
    g.addColorStop(0, 'rgba(37,99,235,0.14)'); g.addColorStop(1, 'rgba(37,99,235,0)');
    ctx.fillStyle = g; ctx.fill(area);
    ctx.strokeStyle = RAMP[0]; ctx.lineWidth = 2; ctx.lineJoin = ctx.lineCap = 'round'; ctx.stroke(path);
    ctx.fillStyle = RAMP[0]; ctx.beginPath(); ctx.arc(ex, ey, 3.5, 0, Math.PI * 2); ctx.fill();
    ctx.fillStyle = '#fff'; ctx.beginPath(); ctx.arc(ex, ey, 1.6, 0, Math.PI * 2); ctx.fill();
  };
  const ref = useCanvasLoop(draw);
  return <canvas ref={ref} className="absolute inset-0 w-full h-full block" />;
}
