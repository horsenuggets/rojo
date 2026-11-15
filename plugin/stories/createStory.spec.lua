--[[

createStory.spec

Tests the stories made by createStory the way UI Labs drives them. Each test renders into a
frame, pushes control changes through `subscribe`, and runs the cleanup function.

--]]

return function()
	const Roact = require("../Packages/Roact")
	const Theme = require("../Plugin/App/Theme")

	const createStory = require("./createStory")

	-- Stands in for UI Labs. It calls `render` with the story's default control values and
	-- hands back the subscribed listener so tests can push control changes.
	const function mountStory(story)
		const target = Instance.new("Frame")
		local listener = nil

		const cleanup = story.render({
			controls = table.clone(story.controls),
			subscribe = function(callback)
				listener = callback
				return function() end
			end,
			target = target,
		})

		return {
			cleanup = cleanup,
			push = function(changes)
				const values = table.clone(story.controls)
				for name, value in changes do
					values[name] = value
				end
				listener(values)
			end,
			target = target,
		}
	end

	const function createLabelStory(seen)
		return createStory({
			controls = {
				Text = "Hello",
			},
			render = function(controls, transparency)
				table.insert(seen, controls)
				return Theme.with(function(theme)
					return Roact.createElement("TextLabel", {
						BackgroundColor3 = theme.BrandColor,
						Text = controls.Text,
						TextTransparency = transparency,
					})
				end)
			end,
		})
	end

	const function findLabel(target)
		return target:FindFirstChildWhichIsA("TextLabel", true)
	end

	-- Stories render on their own thread and the theme provider preloads fonts, so renders
	-- can finish a few frames after the call that started them.
	const function waitUntil(condition)
		const deadline = os.clock() + 5
		while not condition() do
			assert(os.clock() < deadline, "Timed out waiting for the story to render")
			task.wait()
		end
	end

	const function waitForText(target, text)
		waitUntil(function()
			const label = findLabel(target)
			return label ~= nil and label.Text == text
		end)
		return findLabel(target)
	end

	it("should add the shared controls to the story's own controls", function()
		const story = createLabelStory({})

		expect(story.controls.BrandColor).to.equal(Theme.BrandColor)
		expect(story.controls.Transparency).to.equal(0)
		expect(story.controls.Text).to.equal("Hello")
	end)

	it("should let a story override a shared control's default", function()
		const story = createStory({
			controls = {
				Transparency = 0.5,
			},
			render = function()
				return nil
			end,
		})

		expect(story.controls.Transparency).to.equal(0.5)
	end)

	it("should mount the story into the target", function()
		const mounted = mountStory(createLabelStory({}))

		const label = waitForText(mounted.target, "Hello")
		expect(label.BackgroundColor3).to.equal(Theme.BrandColor)

		mounted.cleanup()
	end)

	it("should rerender when a control changes", function()
		const seen = {}
		const mounted = mountStory(createLabelStory(seen))
		waitForText(mounted.target, "Hello")

		mounted.push({ Text = "Changed" })

		waitForText(mounted.target, "Changed")
		expect(seen[#seen].Text).to.equal("Changed")

		mounted.cleanup()
	end)

	it("should apply the latest values when controls change mid-render", function()
		const mounted = mountStory(createLabelStory({}))

		mounted.push({ Text = "First" })
		mounted.push({ Text = "Second" })

		waitForText(mounted.target, "Second")

		mounted.cleanup()
	end)

	it("should recolor the brand color through the theme", function()
		const mounted = mountStory(createLabelStory({}))
		waitForText(mounted.target, "Hello")
		const newColor = Color3.fromHex("FF5D5D")

		mounted.push({ BrandColor = newColor, Text = "Recolored" })

		expect(waitForText(mounted.target, "Recolored").BackgroundColor3).to.equal(newColor)

		mounted.cleanup()
	end)

	it("should drive transparency through a binding", function()
		const mounted = mountStory(createLabelStory({}))
		waitForText(mounted.target, "Hello")

		mounted.push({ Transparency = 0.25, Text = "Faded" })

		expect(waitForText(mounted.target, "Faded").TextTransparency).to.be.near(0.25)

		mounted.cleanup()
	end)

	it("should remove everything from the target on cleanup", function()
		const mounted = mountStory(createLabelStory({}))
		waitForText(mounted.target, "Hello")

		mounted.cleanup()

		expect(#mounted.target:GetChildren()).to.equal(0)
	end)

	it("should remove everything when cleaned up before the first render finishes", function()
		const mounted = mountStory(createLabelStory({}))

		mounted.cleanup()

		waitUntil(function()
			return #mounted.target:GetChildren() == 0
		end)
		task.wait(0.5)
		expect(#mounted.target:GetChildren()).to.equal(0)
	end)

	it("should ignore control changes after cleanup", function()
		const seen = {}
		const mounted = mountStory(createLabelStory(seen))
		waitForText(mounted.target, "Hello")
		mounted.cleanup()

		const renders = #seen
		mounted.push({ Text = "Too late" })
		task.wait(0.5)

		expect(#seen).to.equal(renders)
		expect(#mounted.target:GetChildren()).to.equal(0)
	end)

	it("should reject options without a render function", function()
		expect(function()
			createStory({})
		end).to.throw()
	end)
end
