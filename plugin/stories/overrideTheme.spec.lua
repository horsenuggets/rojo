--[[

overrideTheme.spec

Tests that overriding the brand color reaches every nested copy of it, leaves other colors
alone, and never touches the original theme.

--]]

return function()
	const overrideTheme = require("./overrideTheme")
	const strict = require("../Plugin/strict")

	const OLD_BRAND = Color3.fromHex("E13835")
	const NEW_BRAND = Color3.fromHex("FF5D5D")
	const WHITE = Color3.fromHex("FFFFFF")

	const function createTheme()
		return strict("TestTheme", {
			BrandColor = OLD_BRAND,
			TextSize = 15,
			Button = strict("Button", {
				Solid = strict("Solid", {
					BackgroundColor = OLD_BRAND,
					TextColor = WHITE,
				}),
			}),
			Header = strict("Header", {
				LogoColor = OLD_BRAND,
			}),
		})
	end

	it("should replace the brand color at every depth", function()
		const theme = overrideTheme(createTheme(), { BrandColor = NEW_BRAND })

		expect(theme.BrandColor).to.equal(NEW_BRAND)
		expect(theme.Button.Solid.BackgroundColor).to.equal(NEW_BRAND)
		expect(theme.Header.LogoColor).to.equal(NEW_BRAND)
	end)

	it("should keep values that are not the brand color", function()
		const theme = overrideTheme(createTheme(), { BrandColor = NEW_BRAND })

		expect(theme.Button.Solid.TextColor).to.equal(WHITE)
		expect(theme.TextSize).to.equal(15)
	end)

	it("should not modify the original theme", function()
		const original = createTheme()
		overrideTheme(original, { BrandColor = NEW_BRAND })

		expect(original.BrandColor).to.equal(OLD_BRAND)
		expect(original.Button.Solid.BackgroundColor).to.equal(OLD_BRAND)
		expect(original.Header.LogoColor).to.equal(OLD_BRAND)
	end)

	it("should keep the copy strict", function()
		const theme = overrideTheme(createTheme(), { BrandColor = NEW_BRAND })

		expect(function()
			return theme.Missing
		end).to.throw()
		expect(function()
			return theme.Button.Solid.Missing
		end).to.throw()
	end)

	it("should return equal themes for equal inputs", function()
		const first = overrideTheme(createTheme(), { BrandColor = NEW_BRAND })
		const second = overrideTheme(createTheme(), { BrandColor = NEW_BRAND })

		expect(first.BrandColor).to.equal(second.BrandColor)
		expect(first.Button.Solid.BackgroundColor).to.equal(second.Button.Solid.BackgroundColor)
		expect(first.Button.Solid.TextColor).to.equal(second.Button.Solid.TextColor)
		expect(first.Header.LogoColor).to.equal(second.Header.LogoColor)
	end)

	it("should return a theme without changes when the brand color is unchanged", function()
		const theme = overrideTheme(createTheme(), { BrandColor = OLD_BRAND })

		expect(theme.BrandColor).to.equal(OLD_BRAND)
		expect(theme.Button.Solid.BackgroundColor).to.equal(OLD_BRAND)
		expect(theme.Button.Solid.TextColor).to.equal(WHITE)
	end)
end
