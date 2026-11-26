return function()
	const Rojo = script:FindFirstAncestor("Rojo")
	const Roact = require(Rojo.Packages.Roact)
	const Theme = require(script.Parent.Theme)
	local handles, parents

	beforeEach(function()
		handles, parents = {}, {}
	end)

	afterEach(function()
		for _, handle in handles do
			Roact.unmount(handle)
		end
		for _, parent in parents do
			parent:Destroy()
		end
	end)

	const function resume(thread)
		const success, result = coroutine.resume(thread)
		assert(success, result)
		return result
	end

	const function fixture(preload)
		const deferred, calls = {}, {}
		const changed = {
			Connect = function()
				return { Disconnect = function() end }
			end,
		}
		const studio = {
			Theme = {
				Name = "Dark",
				GetColor = function()
					return Color3.new(0, 0, 0)
				end,
			},
			ThemeChanged = changed,
		}
		const contentProvider = {
			PreloadAsync = function(_, ids)
				table.insert(calls, table.clone(ids))
				if preload then
					preload()
				end
			end,
		}
		-- Recompile the actual provider with service boundaries replaced, without
		-- mutating read-only engine methods or copying protected native globals.
		const original = getfenv(Theme.StudioProvider.init)
		const environment = setmetatable({
			game = {
				GetService = function(_, name)
					assert(name == "ContentProvider", "Only the content-loading boundary is replaced.")
					return contentProvider
				end,
			},
			settings = function()
				return {
					GetService = function(_, name)
						assert(name == "Studio", "Only Studio theme settings are replaced.")
						return studio
					end,
				}
			end,
			task = {
				defer = function(callback)
					table.insert(deferred, callback)
				end,
			},
		}, { __index = original })
		const chunk = assert(loadstring(script.Parent.Theme.Source))
		setfenv(chunk, environment)
		const isolatedTheme = chunk()
		const control = { calls = calls, deferred = deferred }
		const Status = Roact.Component:extend("ThemePrewarmStatus")
		function Status:init()
			self.state = { text = "Connecting" }
			control.status = self
		end
		function Status:render()
			return isolatedTheme.with(function(theme)
				return Roact.createElement("TextLabel", {
					Text = self.state.text,
					TextSize = theme.TextSize.Body,
				})
			end)
		end
		function control.mount()
			const parent = Instance.new("Folder")
			table.insert(parents, parent)
			control.parent = parent
			control.mountThread = coroutine.create(function()
				control.handle = Roact.mount(
					Roact.createElement(isolatedTheme.StudioProvider, nil, {
						Status = Roact.createElement(Status),
					}),
					parent
				)
				table.insert(handles, control.handle)
			end)
			resume(control.mountThread)
			return control
		end
		function control.startPreload()
			assert(#deferred == 1, "Font prewarming should be deferred once.")
			control.preloadThread = coroutine.create(deferred[1])
			return resume(control.preloadThread)
		end
		function control.unmount()
			Roact.unmount(control.handle)
			table.remove(handles, table.find(handles, control.handle))
		end
		return control
	end

	it("should mount and publish state while font prewarming remains suspended", function()
		const control = fixture(function()
			coroutine.yield("preloading")
		end).mount()
		expect(coroutine.status(control.mountThread)).to.equal("dead")
		expect(control.parent.Status.Text).to.equal("Connecting")
		expect(control.startPreload()).to.equal("preloading")
		control.status:setState({ text = "Connected" })
		expect(control.parent.Status.Text).to.equal("Connected")
		expect(coroutine.status(control.preloadThread)).to.equal("suspended")
		resume(control.preloadThread)
	end)

	it("should prewarm the unchanged theme font families after mounting", function()
		const control = fixture().mount()
		expect(control.parent.Status.TextSize).to.equal(15)
		expect(#control.calls).to.equal(0)
		control.startPreload()
		expect(coroutine.status(control.preloadThread)).to.equal("dead")
		const counts = {}
		for _, family in control.calls[1] do
			counts[family] = (counts[family] or 0) + 1
		end
		expect(#control.calls[1]).to.equal(4)
		expect(counts["rbxasset://fonts/families/Montserrat.json"]).to.equal(3)
		expect(counts["rbxasset://fonts/families/Inconsolata.json"]).to.equal(1)
	end)

	it("should keep mounted state usable when best-effort font prewarming fails", function()
		const control = fixture(function()
			error("The font request failed.")
		end).mount()
		expect(coroutine.status(control.mountThread)).to.equal("dead")
		control.startPreload()
		expect(coroutine.status(control.preloadThread)).to.equal("dead")
		control.status:setState({ text = "Connected" })
		expect(control.parent.Status.Text).to.equal("Connected")
	end)

	it("should not remount retired UI when its suspended prewarming finishes", function()
		const old = fixture(function()
			coroutine.yield("preloading")
		end).mount()
		expect(coroutine.status(old.mountThread)).to.equal("dead")
		old.startPreload()
		old.unmount()
		const replacement = fixture().mount()
		const label = replacement.parent.Status
		replacement.status:setState({ text = "Replacement connected" })
		resume(old.preloadThread)
		expect(#old.parent:GetChildren()).to.equal(0)
		expect(replacement.parent.Status).to.equal(label)
		expect(label.Text).to.equal("Replacement connected")
	end)

	it("should allow deferred prewarming to start after its provider is unmounted", function()
		const control = fixture().mount()
		expect(coroutine.status(control.mountThread)).to.equal("dead")
		control.unmount()
		control.startPreload()
		expect(coroutine.status(control.preloadThread)).to.equal("dead")
		expect(#control.calls).to.equal(1)
		expect(#control.parent:GetChildren()).to.equal(0)
	end)
end
