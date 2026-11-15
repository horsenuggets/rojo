--[[

Checkbox.story

Previews the settings checkbox in its active, inactive, and locked states.

--]]

const Roact = require("../../Packages/Roact")
const Checkbox = require("../../Plugin/App/Components/Checkbox")

const createStory = require("../createStory")

return createStory({
	summary = "The toggle used by boolean settings. Hover a locked checkbox to see its tooltip.",
	controls = {
		Active = true,
		Locked = false,
		LockedTooltip = "This setting is locked",
	},
	render = function(controls, transparency)
		return Roact.createElement(Checkbox, {
			active = controls.Active,
			anchorPoint = Vector2.new(0.5, 0.5),
			locked = controls.Locked,
			lockedTooltip = controls.LockedTooltip,
			onClick = function() end,
			position = UDim2.fromScale(0.5, 0.5),
			transparency = transparency,
		})
	end,
})
