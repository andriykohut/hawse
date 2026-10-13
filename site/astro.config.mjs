// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';

const site = 'https://andriykohut.github.io';
const base = '/hawse';

export default defineConfig({
	site,
	base,
	integrations: [
		starlight({
			title: 'hawse',
			description: 'Reverse TCP and UDP tunnel over QUIC with Ed25519 authentication',
			logo: {
				light: '../assets/hawse-lockup.svg',
				dark: '../assets/hawse-lockup-inverse.svg',
				replacesTitle: true,
			},
			favicon: '/favicon.svg',
			head: [
				// A link preview takes its image from an absolute URL, and Starlight names none.
				{
					tag: 'meta',
					attrs: { property: 'og:image', content: `${site}${base}/social-preview.png` },
				},
				{ tag: 'meta', attrs: { property: 'og:image:alt', content: 'The hawse mark' } },
				// Google Search Console rechecks this tag, and the site loses its verification without it.
				{
					tag: 'meta',
					attrs: {
						name: 'google-site-verification',
						content: 'Crai98VFB5q086usyEy0Fy5CKhm19-p1hE8SyXaEG8k',
					},
				},
			],
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/andriykohut/hawse' }],
			customCss: ['@fontsource-variable/archivo', './src/styles/theme.css'],
			components: { Hero: './src/components/Hero.astro' },
			sidebar: [
				{ label: 'Install', slug: 'install' },
				{ label: 'Usage', slug: 'usage' },
				{ label: 'Configuration', slug: 'configuration' },
				{ label: 'Logging', slug: 'logging' },
				{ label: 'Measurements', slug: 'measurements' },
			],
			plugins: [starlightLinksValidator()],
		}),
	],
});
