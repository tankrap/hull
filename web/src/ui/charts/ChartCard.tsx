// keel/hull ChartCard — shared frame for every data graphic (Tailwind).
// White card in a #F7F8FA frame, header row, hero number, optional PillSwitcher footer.
import { PillSwitcher } from '../Tabs';

export function ChartFrame({ children, footer }: { children?: React.ReactNode; footer?: React.ReactNode }) {
  return (
    <div className="bg-frame border border-[oklch(0.92_0.004_250)] rounded-frame p-1.5 flex flex-col gap-1.5">
      {children}
      {footer && <div className="flex justify-center pt-0.5 pb-1">{footer}</div>}
    </div>
  );
}

export function ChartCard({ icon, title, context, hero, heroUnit, delta, deltaTone = 'text-[#2563EB]', children }: {
  icon?: React.ReactNode;
  title?: React.ReactNode;
  context?: React.ReactNode;
  hero?: React.ReactNode;
  heroUnit?: React.ReactNode;
  delta?: React.ReactNode;
  deltaTone?: string;
  children?: React.ReactNode;
}) {
  return (
    <div className="bg-white rounded-[10px] shadow-chart-card p-4">
      <div className="flex justify-between items-center">
        <div className="flex items-center gap-[7px]">
          {icon}
          <span className="text-[13px] font-medium text-[#171717]">{title}</span>
        </div>
        <span className="text-xs text-[#8A90A0]">{context}</span>
      </div>
      {hero != null && (
        <div className="flex items-baseline gap-2 mt-2.5">
          <span className="text-[clamp(26px,3.6vw,31px)] font-semibold text-[#171717] tracking-tight tabular-nums">{hero}</span>
          {delta && <span className={`text-xs font-semibold ${deltaTone}`}>{delta}</span>}
          {heroUnit && <span className="text-xs text-[#8A90A0]">{heroUnit}</span>}
        </div>
      )}
      {children}
    </div>
  );
}

export { PillSwitcher };

// Chart cards stay LIGHT in both themes (hex, not vars). Blue ramp only:
export const RAMP = ['#2563EB', '#4C7BEF', '#6E96F3', '#93B4F6', '#C7D8FA'];
export const REST = '#EEF0F3';

// Hover readout bubble (position it absolutely inside a relative plot).
export function Readout({ x, y, visible, children }: {
  x?: number | string;
  y?: number | string;
  visible?: boolean;
  children?: React.ReactNode;
}) {
  return (
    <div style={{ left: x, top: y }}
      className={`absolute -translate-x-1/2 -translate-y-[135%] bg-white border border-[#E5E7EB] rounded-[10px]
        shadow-[0_6px_20px_-6px_rgba(15,23,42,0.22)] px-[11px] py-[7px] whitespace-nowrap pointer-events-none
        transition-opacity duration-150 ease-out ${visible ? 'opacity-100' : 'opacity-0'}`}>
      {children}
    </div>
  );
}
