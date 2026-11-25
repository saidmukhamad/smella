import React, { lazy as split, memo } from "react";

export const Catalog = split(() => import("./Catalog"));
export const Preview = React.lazy(() => import("./panels").then(p => ({ default: p.PreviewPanel })));
export const Unopened = memo(() => <Archive />);
const Archive = split(() => import("./Archive"));
