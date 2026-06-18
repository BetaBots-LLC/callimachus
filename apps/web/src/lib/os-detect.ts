// Pure OS helpers. Detection runs server-side from the request User-Agent (see
// `getPrimaryOs` in server/releases.ts), so the right download is chosen before
// the first byte — no client state, no effect, no hydration flash.

export type OsKey = "mac_arm" | "win" | "linux_appimage" | "linux_deb";

const FAMILY: Record<OsKey, string> = {
  mac_arm: "macOS",
  win: "Windows",
  linux_appimage: "Linux",
  linux_deb: "Linux",
};

export const osFamily = (key: OsKey): string => FAMILY[key];

/** Map a User-Agent string to the best default download. Unknown → macOS arm. */
export function osFromUserAgent(ua: string): OsKey {
  const p = ua.toLowerCase();
  if (/windows|win32|win64/.test(p)) return "win";
  if (/android/.test(p)) return "mac_arm"; // no mobile build; fall back to the default CTA
  if (/linux|x11|ubuntu|fedora|debian/.test(p)) return "linux_appimage";
  return "mac_arm";
}
