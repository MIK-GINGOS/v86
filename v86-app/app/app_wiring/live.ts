// The broker between screens (mixes) and services (gates). A Figma text layer named
// "@order.status" is read from here as live['order.status']. Screens only see plain
// strings; they never call a service themselves.
import { useEffect, useState } from 'react';
import { fetchOrder } from '../../gates/gm_api/order';

// One entry per service: "order" -> gates/gm_api/order.ts.
const sources: Record<string, () => Promise<object>> = {
  order: fetchOrder,
};

// Polls the services the screen's keys name, so values stay current.
export function useLive(keys: string[], everyMs = 3000): Record<string, string | undefined> {
  const [values, setValues] = useState<Record<string, string | undefined>>({});
  const wanted = keys.join('|');

  useEffect(() => {
    const names = [...new Set(wanted.split('|').map((k) => k.split('.')[0]))].filter((n) => n in sources);
    let alive = true;
    const load = async () => {
      const next: Record<string, string | undefined> = {};
      for (const name of names) {
        try {
          const data = (await sources[name]()) as Record<string, unknown>;
          for (const key of wanted.split('|')) {
            const [source, ...path] = key.split('.');
            if (source !== name) continue;
            let v: unknown = data;
            for (const p of path) v = v && typeof v === 'object' ? (v as Record<string, unknown>)[p] : undefined;
            if (v !== undefined && v !== null) next[key] = String(v);
          }
        } catch (_e) {
          // Service unreachable: the screen keeps showing its Figma text.
        }
      }
      if (alive) setValues(next);
    };
    load();
    const timer = setInterval(load, everyMs);
    return () => { alive = false; clearInterval(timer); };
  }, [wanted, everyMs]);

  return values;
}
