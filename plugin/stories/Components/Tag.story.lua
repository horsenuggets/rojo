--[[

Tag.story

Previews the small colored tag shown next to experimental and debug settings.

--]]

const Roact = require("../../Packages/Roact")
const Assets = require("../../Plugin/Assets")
const Tag = require("../../Plugin/App/Components/Tag")

const createStory = require("../createStory")

return createStory({
	summary = "A labeled tag with an optional icon.",
	controls = {
		Color = Color3.fromHex("F1C45C"),
		ShowIcon = true,
		Text = "Experimental",
	},
	render = function(controls, transparency)
		return Roact.createElement(Tag, {
			anchorPoint = Vector2.new(0.5, 0.5),
			color = controls.Color,
			icon = if controls.ShowIcon then Assets.Images.Icons.Warning else nil,
			position = UDim2.fromScale(0.5, 0.5),
			text = controls.Text,
			transparency = transparency,
		})
	end,
})
