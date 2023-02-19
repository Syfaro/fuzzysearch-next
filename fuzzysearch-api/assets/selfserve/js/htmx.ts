// I don't know why it has to be done like this. From some limited testing, it
// works as one might hope (import htmx from "htmx.org") using webpack, but not
// esbuild. This is a gross workaround but it does work, and I already spent too
// much time trying to figure it out.

import "htmx.org";
import * as HtmxType from "htmx.org";

declare const htmx: typeof HtmxType;

export default htmx;
