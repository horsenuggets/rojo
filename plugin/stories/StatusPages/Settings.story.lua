--[[

Settings.story

Previews the settings page with every setting at its current value.

--]]

const Roact = require("../../Packages/Roact")
const Settings = require("../../Plugin/App/StatusPages/Settings")

const createStory = require("../createStory")

return createStory({
	summary = "The settings page. Locked settings show what's disabled while syncing.",
	controls = {
		SyncActive = false,
	},
	size = UDim2.fromOffset(320, 420),
	render = function(controls, transparency)
		return Roact.createElement(Settings, {
			onBack = function() end,
			syncActive = controls.SyncActive,
			transparency = transparency,
		})
	end,
})
