
// this file is generated — do not edit it


declare module "svelte/elements" {
	export interface HTMLAttributes<T> {
		'data-sveltekit-keepfocus'?: true | '' | 'off' | undefined | null;
		'data-sveltekit-noscroll'?: true | '' | 'off' | undefined | null;
		'data-sveltekit-preload-code'?:
			| true
			| ''
			| 'eager'
			| 'viewport'
			| 'hover'
			| 'tap'
			| 'off'
			| undefined
			| null;
		'data-sveltekit-preload-data'?: true | '' | 'hover' | 'tap' | 'off' | undefined | null;
		'data-sveltekit-reload'?: true | '' | 'off' | undefined | null;
		'data-sveltekit-replacestate'?: true | '' | 'off' | undefined | null;
	}
}

export {};


declare module "$app/types" {
	export interface AppTypes {
		RouteId(): "/" | "/config" | "/cron" | "/hooks" | "/mcp" | "/memory" | "/nodes" | "/sessions" | "/skills";
		RouteParams(): {
			
		};
		LayoutParams(): {
			"/": Record<string, never>;
			"/config": Record<string, never>;
			"/cron": Record<string, never>;
			"/hooks": Record<string, never>;
			"/mcp": Record<string, never>;
			"/memory": Record<string, never>;
			"/nodes": Record<string, never>;
			"/sessions": Record<string, never>;
			"/skills": Record<string, never>
		};
		Pathname(): "/" | "/config" | "/cron" | "/hooks" | "/mcp" | "/memory" | "/nodes" | "/sessions" | "/skills";
		ResolvedPathname(): `${"" | `/${string}`}${ReturnType<AppTypes['Pathname']>}`;
		Asset(): string & {};
	}
}