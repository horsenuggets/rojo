--[[

Header.story

Previews the header with the Rojo+ logo shown at the top of most pages.

--]]

const Roact = require("../../Packages/Roact")
const Header = require("../../Plugin/App/Components/Header")

const createStory = require("../createStory")

return createStory({
	summary = "The page header. The logo is a baked image, so the brand color control only "
		.. "affects the update indicator dot.",
	size = UDim2.fromOffset(320, 72),
	render = function(_controls, transparency)
		return Roact.createElement(Header, {
			transparency = transparency,
		})
	end,
})
