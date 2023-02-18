import * as esbuild from "esbuild";

const isProd = process.env.NODE_ENV === 'production';

const drop = isProd ? ["console"] : [];

await esbuild.build({
  entryPoints: ["assets/js/selfserve/selfserve.ts"],
  outdir: "dist/",
  bundle: true,
  sourcemap: true,
  minify: isProd,
  drop,
});
