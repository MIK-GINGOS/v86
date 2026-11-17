// gm_api: calls the v86 server. Screens never import this; app/app_wiring/live.ts does.
// On a phone, set EXPO_PUBLIC_V86_SERVER to http://<computer-ip>:4000 (localhost is the phone itself).
const BASE = process.env.EXPO_PUBLIC_V86_SERVER ?? 'http://localhost:4000';

// Demo order: a pretend exchange reply from the server. No API key lives in the app.
export async function fetchOrder(): Promise<object> {
  const res = await fetch(`${BASE}/v1/demo/order`);
  if (!res.ok) throw new Error(`order failed: ${res.status}`);
  return res.json();
}
