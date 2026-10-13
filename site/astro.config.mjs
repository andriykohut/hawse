// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';

export default defineConfig({
	site: 'https://andriykohut.github.io',
	base: '/hawse',
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
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/andriykohut/hawse' }],
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
