import { quote } from "@domain/pricing";
export function PreviewPanel() { return <aside>{quote()}</aside>; }
export function OtherPanel() { return null; }
