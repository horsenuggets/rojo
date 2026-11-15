--[[

overrideTheme

Returns a copy of a plugin theme with the brand color swapped out. The brand color is
copied into many theme entries (solid buttons, checkboxes, the spinner, the header logo),
so every Color3 that matches the original brand color is replaced, not just the top-level
`BrandColor` entry. The input theme is never modified.

--]]

export type Overrides = {
	BrandColor: Color3,
}

const function replaceColors(value: any, from: Color3, to: Color3): any
	if typeof(value) == "Color3" then
		return if value == from then to else value
	end

	if type(value) ~= "table" then
		return value
	end

	-- Theme tables are strict and read-only, so the copy keeps their metatable and fills
	-- in its entries with rawset.
	const copy = setmetatable({}, getmetatable(value))
	for key, child in value do
		rawset(copy, key, replaceColors(child, from, to))
	end

	return copy
end

const function overrideTheme(theme: { [string]: any }, overrides: Overrides): { [string]: any }
	assert(type(theme) == "table", "Expected theme to be a table")
	assert(typeof(theme.BrandColor) == "Color3", "Expected theme to have a BrandColor")
	assert(typeof(overrides.BrandColor) == "Color3", "Expected overrides.BrandColor to be a Color3")

	return replaceColors(theme, theme.BrandColor, overrides.BrandColor)
end

return overrideTheme
