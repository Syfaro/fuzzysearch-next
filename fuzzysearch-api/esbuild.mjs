import autoprefixer from "autoprefixer";
import * as esbuild from "esbuild";
import { sassPlugin } from "esbuild-sass-plugin";
import postcss from "postcss";
import postcssPresetEnv from "postcss-preset-env";

const isProd = process.env.NODE_ENV === "production";

await esbuild.build({
  entryPoints: [
    "assets/selfserve/css/selfserve.scss",
    "assets/selfserve/js/selfserve.ts",
  ],
  outdir: "dist/",
  outbase: "assets/",
  bundle: true,
  sourcemap: true,
  minify: isProd,
  drop: isProd ? ["console"] : [],
  plugins: [
    sassPlugin({
      async transform(source, resolveDir) {
        const { css } = await postcss([
          autoprefixer,
          postcssPresetEnv({ stage: 0 }),
        ]).process(source, { from: undefined });
        return css;
      },
    }),
  ],
});
