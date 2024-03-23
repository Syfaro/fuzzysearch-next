import autoprefixer from "autoprefixer";
import * as esbuild from "esbuild";
import { copy } from "esbuild-plugin-copy";
import { sassPlugin } from "esbuild-sass-plugin";
import postcss from "postcss";
import postcssPresetEnv from "postcss-preset-env";

const isProd = process.env.NODE_ENV === "production";

await esbuild.build({
  entryPoints: ["assets/selfserve/js/selfserve.ts"],
  outdir: "dist/",
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
    copy({
      assets: [
        {
          from: ["assets/img/**/*"],
          to: ["./img"],
        },
      ],
    }),
  ],
});
