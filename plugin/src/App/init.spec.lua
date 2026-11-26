return function()
	local App = require(script.Parent)({}).component

	it("should not show sync reminders for the fork", function()
		expect(App.checkSyncReminder({})).to.equal(nil)
		expect(App.startSyncReminderPolling({})).to.equal(nil)
		expect(App.sendSyncReminder({}, "Test", {})).to.equal(nil)
	end)

	-- Only the session the place was built for gets the live edit behavior, so a
	-- manual session to another project keeps the user's settings and confirmation.
	it("should only treat the marked endpoint as live edit", function()
		local marked = {
			liveEditConnection = {
				host = "localhost",
				port = "34901",
			},
		}
		expect(App.isLiveEditEndpoint(marked, "localhost", "34901")).to.equal(true)
		expect(App.isLiveEditEndpoint(marked, "localhost", 34901)).to.equal(true)
		expect(App.isLiveEditEndpoint(marked, "localhost", "34872")).to.equal(false)
		expect(App.isLiveEditEndpoint(marked, "127.0.0.1", "34901")).to.equal(false)
		expect(App.isLiveEditEndpoint({}, "localhost", "34901")).to.equal(false)
	end)

	-- A bare host and its "http://" spelling reach the same server, in either
	-- the marker or the session. HTTPS is a different endpoint.
	it("should treat an http prefix as the same live edit host", function()
		for _, markerHost in { "localhost", "http://localhost" } do
			local marked = {
				liveEditConnection = {
					host = markerHost,
					port = "34901",
				},
			}
			expect(App.isLiveEditEndpoint(marked, "localhost", "34901")).to.equal(true)
			expect(App.isLiveEditEndpoint(marked, "http://localhost", "34901")).to.equal(true)
			expect(App.isLiveEditEndpoint(marked, "https://localhost", "34901")).to.equal(false)
			expect(App.isLiveEditEndpoint(marked, "http://127.0.0.1", "34901")).to.equal(false)
			expect(App.isLiveEditEndpoint(marked, "http://localhost", "34872")).to.equal(false)
		end
	end)

	-- Hosts are compared as the URLs a session connects to, so stripping a prefix
	-- from a doubled scheme must not turn it into a different, real endpoint.
	it("should not match a doubled scheme with the endpoint inside it", function()
		local marked = {
			liveEditConnection = {
				host = "http://https://localhost",
				port = "34901",
			},
		}
		expect(App.isLiveEditEndpoint(marked, "https://localhost", "34901")).to.equal(false)
		expect(App.isLiveEditEndpoint(marked, "http://https://localhost", "34901")).to.equal(true)

		local secure = {
			liveEditConnection = {
				host = "https://localhost",
				port = "34901",
			},
		}
		expect(App.isLiveEditEndpoint(secure, "https://localhost", "34901")).to.equal(true)
		expect(App.isLiveEditEndpoint(secure, "localhost", "34901")).to.equal(false)
	end)

	describe("session ownership", function()
		local Plugin = script:FindFirstAncestor("Rojo").Plugin
		local ApiContext = require(Plugin.ApiContext)
		local PatchSet = require(Plugin.PatchSet)
		local PatchTree = require(Plugin.PatchTree)
		local InstanceMap = require(Plugin.InstanceMap)
		local Timer = require(Plugin.Timer)
		local createSourceSetter = require(Plugin.Reconciler.setSource)
		local ServeSession = require(Plugin.ServeSession)
		local Settings = require(Plugin.Settings)
		local originalApiNew, originalSessionNew, originalGet
		local app, options, onStart, onStop, onApi
		local patch, instanceMap
		local originalBuild, patchEnvironment, originalOs, originalTask
		local maps, instances

		local function applyState(update)
			local changes = if type(update) == "function" then update(app.state) else update
			for key, value in changes or {} do
				app.state[key] = value
			end
		end

		local function flushState()
			local pending = app.pendingState
			app.pendingState = {}
			for _, update in pending do
				applyState(update)
			end
		end

		local function resume(thread, ...)
			local success, result = coroutine.resume(thread, ...)
			assert(success, result)
			return result
		end

		beforeEach(function()
			maps, instances = {}, {}
			originalBuild = PatchTree.build
			patchEnvironment = getfenv(originalBuild)
			originalOs, originalTask = patchEnvironment.os, patchEnvironment.task
			originalApiNew = ApiContext.new
			originalSessionNew = ServeSession.new
			originalGet = Settings.get
			options = { twoWaySync = false, confirmationBehavior = "Always" }
			Settings.get = function(_, key)
				return options[key]
			end
			onStart, onStop, onApi = nil, nil, nil
			patch = PatchSet.newEmpty()
			table.insert(patch.updated, { id = "missing", changedProperties = {}, changedName = "Changed" })
			instanceMap = { fromIds = {}, fromInstances = {} }
			app = setmetatable({
				-- Marker I/O is separate from the lifetime transitions under test.
				readLiveEditMarker = function(self)
					return self.liveEditConnection
				end,
				knownProjects = {},
				notifications = {},
				pendingState = {},
				state = {
					appStatus = "NotConnected",
					patchData = { patch = PatchSet.newEmpty(), unapplied = PatchSet.newEmpty(), timestamp = 0 },
				},
				confirmationEvent = {
					Wait = function()
						return coroutine.yield("confirming")
					end,
				},
				getHostAndPort = function()
					return "localhost", "34872"
				end,
				isAutoConnectPlaytestServerAvailable = function()
					return false
				end,
				claimSyncLock = function(self)
					self.locked = true
					return true
				end,
				releaseSyncLock = function(self)
					self.locked = false
					if self.onRelease then
						self.onRelease()
					end
				end,
				setPriorSyncInfo = function(self, _, _, name)
					self.priorProject = name
					if self.onPrior then
						self.onPrior()
					end
				end,
				setRunningConnectionInfo = function(self, url)
					self.runningUrl = url
				end,
				clearRunningConnectionInfo = function(self)
					self.runningUrl = nil
					if self.onClear then
						self.onClear()
					end
				end,
				addNotification = function(self, notification)
					table.insert(self.notifications, notification.text)
					if self.onNotification then
						self.onNotification(notification.text)
					end
				end,
				setState = function(self, update)
					if self.deferState then
						table.insert(self.pendingState, update)
					else
						applyState(update)
					end
					if self.onState then
						self.onState()
					end
				end,
			}, { __index = App })
			ApiContext.new = function(url)
				if onApi then
					onApi()
				end
				return { url = url }
			end
			ServeSession.new = function(config)
				local session = { config = config, preActive = false, postActive = false }
				function session:setSyncResultCallback(callback)
					self.result = callback
				end
				function session:setUpdateLoadingTextCallback(callback)
					self.loading = callback
				end
				function session:setConfirmCallback(callback)
					self.confirm = callback
				end
				function session:onStatusChanged(callback)
					self.status = callback
				end
				function session:hookPrecommit(callback)
					self.pre = callback
					self.preActive = true
					return function()
						self.preActive = false
					end
				end
				function session:hookPostcommit(callback)
					self.post = callback
					self.postActive = true
					return function()
						self.postActive = false
					end
				end
				function session:start()
					self.started = true
					self.status(ServeSession.Status.Connecting)
					if onStart then
						onStart(self)
					end
				end
				function session:stop()
					self.stopped = true
					if onStop then
						onStop()
					end
					self.status(ServeSession.Status.Disconnected)
				end
				return session
			end
		end)

		afterEach(function()
			PatchTree.build = originalBuild
			patchEnvironment.os, patchEnvironment.task = originalOs, originalTask
			for _, map in maps do
				map:stop()
			end
			for _, instance in instances do
				instance:Destroy()
			end
			ApiContext.new = originalApiNew
			ServeSession.new = originalSessionNew
			Settings.get = originalGet
		end)

		for _, callback in { "pre", "confirm" } do
			it(`should pass the {callback} ownership predicate into the real PatchTree builder`, function()
				local predicate
				PatchTree.build = function(changes, map, headers, isCurrent)
					predicate = isCurrent
					expect(type(isCurrent)).to.equal("function")
					expect(isCurrent()).to.equal(true)
					return originalBuild(changes, map, headers, isCurrent)
				end
				app:startSession()
				local session = app.serveSession
				if callback == "pre" then
					session.pre(patch, instanceMap)
				else
					local thread = coroutine.create(function()
						return session.confirm(instanceMap, patch, {})
					end)
					expect(resume(thread)).to.equal("confirming")
					expect(resume(thread, "Accept")).to.equal("Accept")
				end
				session.status(ServeSession.Status.Connected, "Current")
				-- Confirmation also belongs to its status transition, not just its session.
				expect(predicate()).to.equal(callback == "pre")
				app:endSession()
				expect(predicate()).to.equal(false)
			end)

			for _, boundary in { 0, 1, 2 } do
				it(
					`should not let a queued {callback} builder at boundary {boundary} contaminate a new owner`,
					function()
						local source = createSourceSetter({
							ConnectChanged = function()
								return { Disconnect = function() end }
							end,
							GetEditorSource = function()
								return "return true"
							end,
							Spawn = function()
								error("No writes should be scheduled.")
							end,
							UpdateSourceAsync = function()
								error("No writes should be invoked.")
							end,
						})
						local parent = Instance.new("Folder")
						table.insert(instances, parent)
						local leaf = Instance.new("ModuleScript")
						leaf.Parent = parent
						instanceMap = InstanceMap.new(nil, nil, source)
						table.insert(maps, instanceMap)
						instanceMap:insert("SCRIPT", leaf)
						patch = PatchSet.newEmpty()
						table.insert(patch.removed, leaf)
						local time, waits = 0, 0
						patchEnvironment.os = setmetatable({
							clock = function()
								time += 0.1
								return time
							end,
						}, { __index = originalOs })
						patchEnvironment.task = setmetatable({
							wait = function()
								waits += 1
								if waits == boundary then
									coroutine.yield("building")
								end
							end,
						}, { __index = originalTask })
						app:startSession()
						local former = app.serveSession
						local thread = coroutine.create(function()
							if callback == "pre" then
								return former.pre(patch, instanceMap)
							end
							return former.confirm(instanceMap, patch, {})
						end)
						if boundary > 0 then
							expect(resume(thread)).to.equal("building")
						end
						app:endSession()
						instanceMap:stop()
						app:startSession()
						local replacement = app.serveSession
						local replacementMap = InstanceMap.new(nil, nil, source)
						table.insert(maps, replacementMap)
						replacementMap:insert("NEW_SCRIPT", leaf)
						local lease = replacementMap.sourceConnections[leaf]
						local state = table.clone(app.state)
						local answer = resume(thread)
						expect(coroutine.status(thread)).to.equal("dead")
						expect(instanceMap:size()).to.equal(0)
						expect(next(instanceMap.sourceConnections)).to.equal(nil)
						expect(next(instanceMap.instancesToSignal)).to.equal(nil)
						expect(replacementMap:getSourceRevision(leaf)).to.equal(0)
						expect(replacementMap.sourceConnections[leaf]).to.equal(lease)
						expect(app.serveSession).to.equal(replacement)
						for key, value in state do
							expect(app.state[key]).to.equal(value)
						end
						expect(app.state.patchTree).to.equal(nil)
						if callback == "confirm" then
							expect(answer).to.equal("Abort")
						end
						expect(Timer._entries[thread]).to.equal(nil)
					end
				)
			end
		end

		it("should not retain a synchronously rejected startup and should preserve its error", function()
			local rejected
			onStart = function(session)
				rejected = session
				session.status(ServeSession.Status.Disconnected, "Exact startup failure")
			end
			app:startSession()
			expect(app.serveSession).to.equal(nil)
			expect(app.state.appStatus).to.equal("Error")
			expect(app.state.errorMessage).to.equal("Exact startup failure")
			expect(app.notifications[#app.notifications]).to.equal("Exact startup failure")
			expect(app.locked).to.equal(false)
			expect(rejected.preActive).to.equal(false)
			expect(rejected.postActive).to.equal(false)
		end)

		it("should disconnect while start is suspended without resurrecting ownership", function()
			local starting
			onStart = function(session)
				starting = session
				coroutine.yield("starting")
			end
			local thread = coroutine.create(function()
				app:startSession()
			end)
			expect(resume(thread)).to.equal("starting")
			expect(app.serveSession).to.equal(starting)
			app:endSession()
			resume(thread)
			expect(app.serveSession).to.equal(nil)
			expect(app.state.appStatus).to.equal("NotConnected")
			expect(starting.stopped).to.equal(true)
			expect(starting.preActive).to.equal(false)
			expect(starting.postActive).to.equal(false)
			expect(app.locked).to.equal(false)
		end)

		it("should reject overlapping starts both during and after startup", function()
			local first
			onStart = function(session)
				first = session
				onStart = nil
				app:startSession()
				expect(app.serveSession).to.equal(session)
			end
			app:startSession()
			app:startSession()
			expect(app.serveSession).to.equal(first)
			expect(first.preActive).to.equal(true)
			expect(app.locked).to.equal(true)
		end)

		it("should reserve startup ownership before constructing the API", function()
			local overlapping
			onApi = function()
				onApi = nil
				app:startSession()
				overlapping = app.serveSession
			end
			app:startSession()
			expect(overlapping).to.equal(nil)
			expect(app.serveSession.started).to.equal(true)
			app:endSession()
			expect(app.locked).to.equal(false)
		end)

		it("should cancel startup while the API constructor is suspended", function()
			onApi = function()
				coroutine.yield("constructing")
			end
			local thread = coroutine.create(function()
				app:startSession()
			end)
			expect(resume(thread)).to.equal("constructing")
			app:endSession()
			onApi = nil
			app:startSession()
			local replacement = app.serveSession
			resume(thread)
			expect(app.serveSession).to.equal(replacement)
			expect(replacement.preActive).to.equal(true)
			expect(app.locked).to.equal(true)
		end)

		it("should not let a lock rejection overwrite a reentrant replacement", function()
			app.claimSyncLock = function()
				return false, "Other editor"
			end
			app.onNotification = function()
				app.onNotification = nil
				app.claimSyncLock = function(self)
					self.locked = true
					return true
				end
				app:startSession()
			end
			app:startSession()
			expect(app.serveSession.started).to.equal(true)
			expect(app.state.appStatus).to.equal("Connecting")
			expect(app.locked).to.equal(true)
		end)

		it("should show the actual lock rejection and permit a later retry", function()
			local claim = app.claimSyncLock
			app.claimSyncLock = function()
				return false, "Other editor"
			end
			app:startSession()
			expect(app.serveSession).to.equal(nil)
			expect(app.state.appStatus).to.equal("Error")
			expect(app.state.errorMessage).to.equal("Could not sync because user 'Other editor' is already syncing")
			expect(app.notifications[#app.notifications]).to.equal(app.state.errorMessage)
			app.claimSyncLock = claim
			app:startSession()
			expect(app.state.appStatus).to.equal("Connecting")
			expect(app.locked).to.equal(true)
		end)

		it("should preserve replacement hooks installed during former hook cleanup", function()
			app:startSession()
			local former = app.serveSession
			local cleanup = app.cleanupPrecommit
			app.cleanupPrecommit = function()
				cleanup()
				app:startSession()
			end
			app:endSession()
			local replacement = app.serveSession
			expect(replacement ~= nil and replacement ~= former).to.equal(true)
			expect(former.preActive).to.equal(false)
			expect(former.postActive).to.equal(false)
			expect(replacement.preActive).to.equal(true)
			expect(replacement.postActive).to.equal(true)
			expect(app.locked).to.equal(true)
			app:endSession()
			expect(replacement.preActive).to.equal(false)
			expect(replacement.postActive).to.equal(false)
		end)

		it("should preserve a replacement started inside stop and clean only the former hooks", function()
			app:startSession()
			local former = app.serveSession
			onStop = function()
				onStop = nil
				app:startSession()
				app.serveSession.status(ServeSession.Status.Connected, "Replacement")
			end
			app:endSession()
			local replacement = app.serveSession
			expect(replacement ~= nil and replacement ~= former).to.equal(true)
			expect(app.state.appStatus).to.equal("Connected")
			expect(app.state.projectName).to.equal("Replacement")
			expect(former.preActive).to.equal(false)
			expect(former.postActive).to.equal(false)
			expect(replacement.preActive).to.equal(true)
			expect(replacement.postActive).to.equal(true)
			expect(app.locked).to.equal(true)
			expect(app.runningUrl).to.equal("http://localhost:34872")
			app:endSession()
			expect(replacement.preActive).to.equal(false)
			expect(replacement.postActive).to.equal(false)
		end)

		for _, boundary in { "onRelease", "onClear", "onState", "onNotification" } do
			it(`should preserve replacement ownership during disconnected {boundary}`, function()
				app:startSession()
				local former = app.serveSession
				app[boundary] = function()
					app[boundary] = nil
					app:startSession()
					app.serveSession.status(ServeSession.Status.Connected, "Replacement")
				end
				former.status(ServeSession.Status.Disconnected, "Former failure")
				local replacement = app.serveSession
				expect(replacement ~= nil and replacement ~= former).to.equal(true)
				expect(app.state.appStatus).to.equal("Connected")
				expect(app.state.projectName).to.equal("Replacement")
				expect(app.locked).to.equal(true)
				expect(app.runningUrl).to.equal("http://localhost:34872")
				expect(replacement.preActive).to.equal(true)
				expect(replacement.postActive).to.equal(true)
				expect(former.preActive).to.equal(false)
				expect(former.postActive).to.equal(false)
			end)
		end

		it("should display overwrite notices from a saved write without changing connection state", function()
			app:startSession()
			const session = app.serveSession
			session.status(ServeSession.Status.Connected, "Current")
			const count = #app.notifications
			session.result("Saved", {
				notices = { { message = 'Studio replaced a filesystem change to "Workspace.Part".' } },
			})
			expect(#app.notifications).to.equal(count + 1)
			expect(app.notifications[#app.notifications]).to.equal(
				'Studio replaced a filesystem change to "Workspace.Part".'
			)
			expect(app.state.appStatus).to.equal("Connected")
			session.result("Saved", {})
			expect(#app.notifications).to.equal(count + 1)
		end)

		it("should ignore every obsolete session callback including automatic confirmation", function()
			app:startSession()
			local former = app.serveSession
			app:endSession()
			app:startSession()
			local replacement = app.serveSession
			replacement.status(ServeSession.Status.Connected, "Replacement")
			replacement.loading("Current loading")
			local state, notifications = table.clone(app.state), table.clone(app.notifications)
			former.status(ServeSession.Status.Connecting)
			former.status(ServeSession.Status.Connected, "Obsolete")
			former.status(ServeSession.Status.Disconnected, "Obsolete failure")
			former.loading("Obsolete loading")
			former.result("Conflicted", {})
			former.result("Failed", { errors = { { message = "Obsolete" } } })
			former.result("Saved", { notices = { { message = "Obsolete overwrite notice." } } })
			former.pre(patch, instanceMap)
			former.post(patch, instanceMap, PatchSet.newEmpty())
			expect(former.confirm(instanceMap, PatchSet.newEmpty(), {})).to.equal("Abort")
			expect(app.serveSession).to.equal(replacement)
			expect(app.knownProjects.Obsolete).to.equal(nil)
			expect(app.priorProject).to.equal("Replacement")
			expect(app.locked).to.equal(true)
			expect(#app.notifications).to.equal(#notifications)
			for key, value in state do
				expect(app.state[key]).to.equal(value)
			end
		end)

		for _, boundary in { "onPrior", "onState", "onNotification" } do
			it(`should fence a connected continuation after reentrant {boundary}`, function()
				app:startSession()
				local former = app.serveSession
				app[boundary] = function()
					app[boundary] = nil
					app:endSession()
					app:startSession()
					app.serveSession.status(ServeSession.Status.Connected, "Replacement")
				end
				former.status(ServeSession.Status.Connected, "Former")
				expect(app.state.projectName).to.equal("Replacement")
				expect(app.state.appStatus).to.equal("Connected")
				expect(app.notifications[#app.notifications]).to.equal(
					"Connected to session 'Replacement' at localhost:34872."
				)
				expect(app.priorProject).to.equal("Replacement")
				expect(app.locked).to.equal(true)
			end)
		end

		it("should not resume a confirmation for a replaced session", function()
			app:startSession()
			local former = app.serveSession
			local answer
			local thread = coroutine.create(function()
				answer = former.confirm(instanceMap, patch, { projectName = "Former" })
			end)
			expect(resume(thread)).to.equal("confirming")
			expect(app.state.appStatus).to.equal("Confirming")
			app:endSession()
			app:startSession()
			resume(thread, "Accept")
			expect(answer).to.equal("Abort")
			expect(app.state.appStatus).to.equal("Connecting")
		end)

		for _, boundary in { "onState", "onNotification" } do
			it(`should abort confirmation before waiting after reentrant {boundary}`, function()
				app:startSession()
				local former = app.serveSession
				app[boundary] = function()
					app[boundary] = nil
					app:endSession()
					app:startSession()
				end
				app.confirmationEvent.Wait = function()
					error("Obsolete confirmation must not wait")
				end
				expect(former.confirm(instanceMap, patch, { projectName = "Former" })).to.equal("Abort")
				expect(app.state.appStatus).to.equal("Connecting")
			end)
		end

		it("should fence deferred loading and commit state from a former session", function()
			app:startSession()
			local former = app.serveSession
			app.deferState = true
			former.loading("Former loading")
			former.pre(patch, instanceMap)
			former.post(patch, instanceMap, PatchSet.newEmpty())
			former.status(ServeSession.Status.Connected, "Former")
			app:endSession()
			app:startSession()
			local replacement = app.serveSession
			app.deferState = false
			replacement.loading("Replacement loading")
			replacement.status(ServeSession.Status.Connected, "Replacement")
			local currentPatchData = app.state.patchData
			flushState()
			expect(app.serveSession).to.equal(replacement)
			expect(app.state.appStatus).to.equal("Connected")
			expect(app.state.projectName).to.equal("Replacement")
			expect(app.state.connectingText).to.equal("Replacement loading")
			expect(app.state.patchData).to.equal(currentPatchData)
			expect(app.state.patchTree).to.equal(nil)
		end)

		it("should not apply queued connecting state after the same session connects", function()
			app.deferState = true
			app:startSession()
			app.deferState = false
			app.serveSession.status(ServeSession.Status.Connected, "Current")
			flushState()
			expect(app.state.appStatus).to.equal("Connected")
			expect(app.state.projectName).to.equal("Current")
		end)

		it("should not resume connecting effects after a reentrant connected status", function()
			local starting
			onStart = function(session)
				starting = session
			end
			app:startSession()
			app.dismissSyncReminder = function()
				starting.status(ServeSession.Status.Connected, "Current")
			end
			starting.status(ServeSession.Status.Connecting)
			expect(app.state.appStatus).to.equal("Connected")
			expect(app.notifications[#app.notifications]).to.equal("Connected to session 'Current' at localhost:34872.")
		end)

		it("should not apply queued connecting state over confirmation", function()
			app.deferState = true
			app:startSession()
			app.deferState = false
			local thread = coroutine.create(function()
				return app.serveSession.confirm(instanceMap, patch, {})
			end)
			expect(resume(thread)).to.equal("confirming")
			flushState()
			expect(app.state.appStatus).to.equal("Confirming")
			expect(resume(thread, "Accept")).to.equal("Accept")
		end)

		it("should not apply queued confirmation state after the session connects", function()
			app:startSession()
			app.deferState = true
			local thread = coroutine.create(function()
				return app.serveSession.confirm(instanceMap, patch, {})
			end)
			expect(resume(thread)).to.equal("confirming")
			app.deferState = false
			expect(resume(thread, "Accept")).to.equal("Accept")
			app.serveSession.status(ServeSession.Status.Connected, "Current")
			flushState()
			expect(app.state.appStatus).to.equal("Connected")
			expect(app.state.projectName).to.equal("Current")
			expect(app.state.confirmData).to.equal(nil)
		end)

		it("should fence deferred confirmation state after disconnect without a replacement", function()
			app:startSession()
			local former = app.serveSession
			app.deferState = true
			local thread = coroutine.create(function()
				former.confirm(instanceMap, patch, {})
			end)
			expect(resume(thread)).to.equal("confirming")
			app:endSession()
			flushState()
			expect(app.state.appStatus).to.equal("NotConnected")
			expect(app.state.confirmData).to.equal(nil)
			expect(app.state.patchTree).to.equal(nil)
			resume(thread, "Abort")
		end)

		it("should retain active loading, commit hooks, confirmation answers and save errors", function()
			app:startSession()
			local session = app.serveSession
			session.loading("Reading project")
			expect(app.state.connectingText).to.equal("Reading project")
			session.pre(patch, instanceMap)
			expect(app.state.patchTree ~= nil).to.equal(true)
			session.post(patch, instanceMap, PatchSet.newEmpty())
			expect(app.state.patchData.patch.updated[1].changedName).to.equal("Changed")
			local answer
			local thread = coroutine.create(function()
				answer = session.confirm(instanceMap, patch, {})
			end)
			expect(resume(thread)).to.equal("confirming")
			resume(thread, "Reject")
			expect(answer).to.equal("Reject")
			session.result(
				"Failed",
				{ errors = { { property = "Source", message = "Disk unavailable" }, { message = "Missing file" } } }
			)
			expect(app.notifications[#app.notifications]).to.equal(
				"Could not save Studio changes:\nSource: Disk unavailable\nInstance: Missing file"
			)
		end)

		it("retains a suspended directional session across App disconnects and replaces only its transport", function()
			local Policy = require(Plugin.DirectionPolicy)
			app.liveEditConnection = Policy.marker("localhost:34872", "session", "studio-to-fs")
			app:startSession()
			local original = app.serveSession
			local key = Policy.resumeKey("http://localhost:34872", "session", "studio-to-fs")
			original.getResumeKey = function()
				return key
			end
			original.resume = function(self, api, marker)
				self.resumedApi, self.resumedMarker = api, marker
			end
			local oldPrecommit = original.pre
			app:endSession()
			expect(app.resumableSessions[key]).to.equal(original)
			expect(original.preActive or original.postActive).to.equal(false)
			app:startSession()
			expect(app.serveSession).to.equal(original)
			expect(original.resumedApi.url).to.equal("http://localhost:34872")
			expect(original.resumedMarker).to.equal(app.liveEditConnection)
			app.state.patchTree = nil
			oldPrecommit(patch, instanceMap)
			expect(app.state.patchTree).to.equal(nil)
			expect(options.twoWaySync).to.equal(false)
		end)

		it("refuses switching endpoints while Studio-only identity state is retained", function()
			app.resumableSessions = { other = {} }
			local disconnected = false
			ApiContext.new = function()
				return {
					disconnect = function()
						disconnected = true
					end,
				}
			end
			app:startSession()
			expect(app.serveSession).to.equal(nil)
			expect(disconnected).to.equal(true)
			expect(app.state.appStatus).to.equal("Error")
			expect(app.state.errorMessage:find("automatic takeover was refused", 1, true) ~= nil).to.equal(true)
		end)

		it("should preserve live edit authority without changing manual session settings", function()
			app.liveEditConnection = { host = "localhost", port = "34872" }
			app:startSession()
			expect(app.serveSession.config.twoWaySync).to.equal(true)
			expect(app.serveSession.config.twoWaySyncAuthority).to.equal("Filesystem")
			expect(app.serveSession.confirm(instanceMap, patch, {})).to.equal("Accept")
			app:endSession()
			app.liveEditConnection = nil
			app:startSession()
			expect(app.serveSession.config.twoWaySync).to.equal(false)
			expect(app.serveSession.config.twoWaySyncAuthority).to.equal(nil)
			expect(options.twoWaySync).to.equal(false)
			local thread = coroutine.create(function()
				return app.serveSession.confirm(instanceMap, patch, {})
			end)
			expect(resume(thread)).to.equal("confirming")
			expect(resume(thread, "Accept")).to.equal("Accept")
		end)
	end)
end
