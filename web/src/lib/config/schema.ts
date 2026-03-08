export type JsonPrimitive = string | number | boolean | null;
export type JsonValue = JsonPrimitive | JsonValue[] | { [key: string]: JsonValue };

export type PathSegment = string | number;

export type JsonSchema = {
	$defs?: Record<string, JsonSchema>;
	$ref?: string;
	additionalProperties?: boolean | JsonSchema;
	allOf?: JsonSchema[];
	anyOf?: JsonSchema[];
	default?: JsonValue;
	description?: string;
	enum?: JsonValue[];
	format?: string;
	items?: JsonSchema | JsonSchema[];
	oneOf?: JsonSchema[];
	properties?: Record<string, JsonSchema>;
	required?: string[];
	title?: string;
	type?: string | string[];
};

export type ConfigCategory = {
	id: string;
	title: string;
	keys: string[];
};

const CATEGORY_RULES = [
	{
		id: 'general',
		title: 'General',
		match: (key: string) =>
			[
				'api_key',
				'api_url',
				'default_provider',
				'default_model',
				'default_temperature',
				'identity'
			].includes(key)
	},
	{
		id: 'provider',
		title: 'Provider',
		match: (key: string) =>
			[
				'reliability',
				'model_routes',
				'embedding_routes',
				'media',
				'web_search'
			].includes(key) || key.includes('provider')
	},
	{
		id: 'memory',
		title: 'Memory',
		match: (key: string) => ['memory', 'storage'].includes(key)
	},
	{
		id: 'skills',
		title: 'Skills',
		match: (key: string) => ['skills', 'skill_rag'].includes(key)
	},
	{
		id: 'channels',
		title: 'Channels',
		match: (key: string) => key === 'channels_config'
	},
	{
		id: 'agent',
		title: 'Agent',
		match: (key: string) =>
			[
				'observability',
				'autonomy',
				'runtime',
				'scheduler',
				'agent',
				'self_system',
				'query_classification',
				'heartbeat',
				'cron',
				'identity_bindings',
				'user_policies',
				'agents'
			].includes(key)
	},
	{
		id: 'sessions',
		title: 'Sessions',
		match: (key: string) => key === 'sessions_spawn'
	},
	{
		id: 'security',
		title: 'Security',
		match: (key: string) =>
			[
				'gateway',
				'webhook',
				'composio',
				'mcp',
				'secrets',
				'browser',
				'http_request',
				'proxy',
				'tunnel',
				'security'
			].includes(key)
	}
] as const;

export function buildCategories(schema: JsonSchema): ConfigCategory[] {
	const keys = Object.keys(schema.properties ?? {});
	const grouped = new Map<string, ConfigCategory>();
	const unmatched: string[] = [];

	for (const key of keys) {
		const rule = CATEGORY_RULES.find((candidate) => candidate.match(key));
		if (!rule) {
			unmatched.push(key);
			continue;
		}

		const existing = grouped.get(rule.id);
		if (existing) {
			existing.keys.push(key);
			continue;
		}

		grouped.set(rule.id, {
			id: rule.id,
			title: rule.title,
			keys: [key]
		});
	}

	const categories = CATEGORY_RULES.map((rule) => grouped.get(rule.id)).filter(
		(category): category is ConfigCategory => Boolean(category && category.keys.length)
	);

	if (unmatched.length) {
		categories.push({
			id: 'advanced',
			title: 'Advanced',
			keys: unmatched
		});
	}

	return categories;
}

export function labelize(key: string): string {
	return key
		.replace(/_/g, ' ')
		.replace(/\b\w/g, (char) => char.toUpperCase());
}

export function resolveSchema(schema: JsonSchema | undefined, rootSchema: JsonSchema): JsonSchema {
	if (!schema) {
		return {};
	}

	if (schema.$ref) {
		const resolved = resolveJsonPointer(rootSchema, schema.$ref);
		if (resolved) {
			return resolveSchema(resolved, rootSchema);
		}
	}

	let current = { ...schema };

	if (current.allOf?.length) {
		current = current.allOf.reduce(
			(merged, item) => mergeSchemas(merged, resolveSchema(item, rootSchema)),
			current
		);
		delete current.allOf;
	}

	const union = current.anyOf ?? current.oneOf;
	if (union?.length) {
		const resolvedUnion = union.map((item) => resolveSchema(item, rootSchema));
		const nullableVariant = resolvedUnion.find((item) => isNullSchema(item));
		const mainVariant = resolvedUnion.find((item) => !isNullSchema(item));
		if (nullableVariant && mainVariant) {
			current = mergeSchemas(current, mainVariant);
			delete current.anyOf;
			delete current.oneOf;
		}
	}

	if (Array.isArray(current.type)) {
		const nonNullTypes = current.type.filter((value) => value !== 'null');
		current = {
			...current,
			type: nonNullTypes.length <= 1 ? nonNullTypes[0] : nonNullTypes
		};
	}

	return current;
}

export function inferDefaultValue(schema: JsonSchema | undefined, rootSchema: JsonSchema): JsonValue {
	const normalized = resolveSchema(schema, rootSchema);

	if (normalized.default !== undefined) {
		return structuredClone(normalized.default);
	}

	if (normalized.enum?.length) {
		return structuredClone(normalized.enum[0]);
	}

	if (normalized.type === 'object' || normalized.properties || isSchemaMap(normalized)) {
		const value: Record<string, JsonValue> = {};
		for (const [key, childSchema] of Object.entries(normalized.properties ?? {})) {
			const child = resolveSchema(childSchema, rootSchema);
			const childDefault = inferDefaultValue(child, rootSchema);
			if (childDefault !== null || (normalized.required ?? []).includes(key)) {
				value[key] = childDefault;
			}
		}
		return value;
	}

	if (normalized.type === 'array') {
		return [];
	}

	if (normalized.type === 'boolean') {
		return false;
	}

	if (normalized.type === 'number' || normalized.type === 'integer') {
		return 0;
	}

	if (normalized.type === 'string') {
		return '';
	}

	return null;
}

export function isNullableSchema(schema: JsonSchema | undefined): boolean {
	if (!schema) {
		return true;
	}

	if (Array.isArray(schema.type) && schema.type.includes('null')) {
		return true;
	}

	const union = schema.anyOf ?? schema.oneOf;
	return Boolean(union?.some((item) => isNullSchema(item)));
}

export function isSensitivePath(path: PathSegment[]): boolean {
	return path.some((segment) => {
		const value = String(segment).toLowerCase();
		return (
			value === 'api_key' ||
			value === 'api_keys' ||
			value === 'token' ||
			value === 'auth_token' ||
			value === 'password' ||
			value === 'secret' ||
			value === 'db_url' ||
			value.endsWith('_api_key') ||
			value.endsWith('_token') ||
			value.endsWith('_secret') ||
			value.endsWith('_password')
		);
	});
}

export function isLongTextField(path: PathSegment[], schema: JsonSchema): boolean {
	const label = path.map((segment) => String(segment)).join('.');
	const description = schema.description?.toLowerCase() ?? '';
	return (
		label.includes('prompt') ||
		label.includes('instructions') ||
		label.includes('description') ||
		description.includes('prompt') ||
		description.includes('multiline')
	);
}

export function isSchemaMap(schema: JsonSchema): boolean {
	return Boolean(
		!schema.properties &&
			schema.type === 'object' &&
			schema.additionalProperties &&
			schema.additionalProperties !== true
	);
}

function isNullSchema(schema: JsonSchema): boolean {
	if (schema.type === 'null') {
		return true;
	}

	return Array.isArray(schema.type) && schema.type.length === 1 && schema.type[0] === 'null';
}

function resolveJsonPointer(rootSchema: JsonSchema, pointer: string): JsonSchema | undefined {
	if (!pointer.startsWith('#/')) {
		return undefined;
	}

	const segments = pointer
		.slice(2)
		.split('/')
		.map((segment) => segment.replace(/~1/g, '/').replace(/~0/g, '~'));

	let current: unknown = rootSchema;
	for (const segment of segments) {
		if (!current || typeof current !== 'object' || !(segment in current)) {
			return undefined;
		}
		current = (current as Record<string, unknown>)[segment];
	}

	return current as JsonSchema;
}

function mergeSchemas(base: JsonSchema, extra: JsonSchema): JsonSchema {
	return {
		...base,
		...extra,
		properties: {
			...(base.properties ?? {}),
			...(extra.properties ?? {})
		},
		required: Array.from(new Set([...(base.required ?? []), ...(extra.required ?? [])]))
	};
}
