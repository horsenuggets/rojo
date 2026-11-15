--[[

Spinner.story

Previews the loading spinner shown while the plugin connects.

--]]

const Roact = require("../../Packages/Roact")
const Spinner = require("../../Plugin/App/Components/Spinner")

const createStory = require("../createStory")

return createStory({
	summary = "The loading spinner. Its moving arc uses the brand color.",
	render = function(_controls, transparency)
		return Roact.createElement(Spinner, {
			anchorPoint = Vector2.new(0.5, 0.5),
			position = UDim2.fromScale(0.5, 0.5),
			transparency = transparency,
		})
	end,
})
