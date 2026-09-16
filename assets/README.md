# Hawse logo assets

| File | Use |
|---|---|
| `hawse-lockup.svg` | Mark and wordmark, light backgrounds |
| `hawse-lockup-inverse.svg` | Mark and wordmark, dark backgrounds |
| `hawse-mark.svg` | Primary mark, light backgrounds |
| `hawse-mark-inverse.svg` | Dark backgrounds |
| `hawse-mark-mono.svg` | Single-color (stamps, embroidery, faxed docs) |
| `hawse-app-icon.svg` | Rounded-square icon, navy ground |
| `hawse-favicon.svg` | 16–32px favicon (simplified, no chain) |

Colors: ink `#0f1b2d` · paper `#f4f1ea` · line `#e8632b`
Wordmark: Archivo 500, +20% letter-spacing, uppercase. In the lockups the
letters are outlined paths, so they render without the font installed.

Clear space: half the mark's height on all sides. Below 20px use
`hawse-favicon.svg` — the chain links fill in. Never recolor the hull; the orange
chain is the only accent.

## Favicon

```html
<link rel="icon" href="/assets/hawse-favicon.svg" type="image/svg+xml">
<link rel="apple-touch-icon" href="/assets/hawse-app-icon.png">
```

`apple-touch-icon` must be PNG (180×180). Generate from the SVG:

```sh
# macOS / Linux, needs librsvg
rsvg-convert -w 180 -h 180 hawse-app-icon.svg -o hawse-app-icon.png
rsvg-convert -w 1024 -h 1024 hawse-app-icon.svg -o hawse-app-icon@1024.png
```

## README banner

The top-level README swaps in the inverse lockup under GitHub's dark theme:

```html
<h1>
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/hawse-lockup-inverse.svg">
    <img src="assets/hawse-lockup.svg" width="296" alt="hawse">
  </picture>
</h1>
```

GitHub strips `<style>` and scripts from README SVGs but renders plain paths and
fills fine — these files are plain paths.
