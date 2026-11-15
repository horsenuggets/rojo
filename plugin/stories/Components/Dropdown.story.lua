--[[

Dropdown.story

Previews the settings dropdown with the options of the confirmation behavior setting.

--]]

const Roact = require("../../Packages/Roact")
const Dropdown = require("../../Plugin/App/Components/Dropdown")

const createStory = require("../createStory")

const OPTIONS = { "Never", "Initial", "Large Changes", "Unlisted PlaceId" }

return createStory({
	summary = "The picker used by settings with a fixed list of options.",
	controls = {
		Active = "Initial",
		Locked = false,
		LockedTooltip = "This setting is locked",
	},
	render = function(controls, transparency)
		return Roact.createElement(Dropdown, {
			active = controls.Active,
			anchorPoint = Vector2.new(0.5, 0),
			locked = controls.Locked,
			lockedTooltip = controls.LockedTooltip,
			onClick = function() end,
			options = OPTIONS,
			position = UDim2.new(0.5, 0, 0, 40),
			transparency = transparency,
		})
	end,
})
