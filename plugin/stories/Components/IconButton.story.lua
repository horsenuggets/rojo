--[[

IconButton.story

Previews the icon button used for the back, close, and reset actions.

--]]

const Roact = require("../../Packages/Roact")
const Assets = require("../../Plugin/Assets")
const IconButton = require("../../Plugin/App/Components/IconButton")

const createStory = require("../createStory")

return createStory({
	summary = "A round icon button with a hover ripple.",
	controls = {
		Color = Color3.fromHex("FFFFFF"),
		IconSize = 24,
	},
	render = function(controls, transparency)
		return Roact.createElement(IconButton, {
			anchorPoint = Vector2.new(0.5, 0.5),
			color = controls.Color,
			icon = Assets.Images.Icons.Back,
			iconSize = controls.IconSize,
			onClick = function() end,
			position = UDim2.fromScale(0.5, 0.5),
			transparency = transparency,
		})
	end,
})
