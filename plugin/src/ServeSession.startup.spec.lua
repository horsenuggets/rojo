return function()
	local ServeSession = require(script.Parent.ServeSession)
	local Promise = require(script.Parent.Parent.Packages.Promise)
	local PatchSet = require(script.Parent.PatchSet)

	-- Only I/O and reconciliation boundaries are replaced. start, initialSync,
	-- syncStep and the publication queue below execute their production code.
	local function fixture()
		local control = { reads = {}, writes = {}, cursors = {}, hydrated = {}, applied = {}, sockets = 0 }
		local function api()
			local context = { __connected = true }
			function context:connect()
				return Promise.resolve({ rootInstanceId = "ROOT", projectName = "Startup" })
			end
			function context:read()
				local read = { context = self }
				read.promise = Promise.new(function(resolve, reject)
					read.resolve, read.reject = resolve, reject
					table.insert(control.reads, read)
				end)
				return read.promise
			end
			function context:setMessageCursor(cursor)
				table.insert(control.cursors, { context = self, cursor = cursor })
			end
			function context:write(patch)
				table.insert(control.writes, { context = self, patch = patch })
				return Promise.resolve()
			end
			function context:disconnect()
				self.__connected = false
			end
			function context:connectWebSocket()
				control.sockets += 1
				return Promise.resolve()
			end
			return context
		end
		local session = setmetatable({
			__apiContext = api(),
			__status = ServeSession.Status.NotStarted,
			__twoWaySync = true,
			__connections = {},
			__sourceApplyBatches = {},
			__remoteSources = {},
			__instanceMap = { fromIds = {}, fromInstances = {}, stop = function() end },
			__changeBatcher = { stop = function() end },
			__updateLoadingText = function() end,
			__applyGameAndPlaceId = function() end,
			__releaseDeferredChanges = function() end,
			__pruneStructuralGuards = function() end,
			__sourceBaselines = {},
			__sourcePendingCounts = {},
			__applyPatch = function(_, patch)
				table.insert(control.applied, patch)
			end,
			__reconciler = {
				hydrate = function(_, instances, _, _, step)
					table.insert(control.hydrated, instances)
					step()
				end,
				diff = function()
					return true, { added = { OLD = {} }, updated = {}, removed = {} }
				end,
			},
		}, ServeSession)
		control.answer = function(index)
			control.reads[index].resolve({ instances = {}, messageCursor = index })
		end
		control.replace = function()
			session.__apiContext = api()
			session:start()
		end
		control.walk = function(boundary)
			-- A deterministic budget, rather than a wall-clock race against yieldBudget.
			session.__syncStep = function(self)
				return ServeSession.__syncStep(self, function()
					boundary()
					return true
				end)
			end
		end
		return session, control
	end

	for _, closed in { false, true } do
		it(`should finalize a current rejected startup read with API closed {closed}`, function()
			local session, control = fixture()
			local released = { map = false, batcher = false, connection = false }
			session.__instanceMap.stop = function()
				released.map = true
			end
			session.__changeBatcher.stop = function()
				released.batcher = true
			end
			table.insert(session.__connections, {
				Disconnect = function()
					released.connection = true
				end,
			})
			local terminalError
			session:onStatusChanged(function(status, detail)
				if status == ServeSession.Status.Disconnected then
					terminalError = detail
				end
			end)
			session:start()
			session.__apiContext.__connected = not closed
			control.reads[1].reject("The initial read exhausted its attempts.")
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(tostring(terminalError):find("The initial read exhausted its attempts.", 1, true)).to.be.ok()
			expect(session.__apiContext.__connected).to.equal(false)
			expect(released.map and released.batcher and released.connection).to.equal(true)
			expect(#control.hydrated).to.equal(0)
			expect(control.sockets).to.equal(0)
		end)
	end

	for _, closed in { false, true } do
		it(`should finalize a cancelled current startup read with API closed {closed}`, function()
			local session, control = fixture()
			session:start()
			session.__apiContext.__connected = not closed
			control.reads[1].promise:cancel()
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(session.__apiContext.__connected).to.equal(false)
			expect(#control.hydrated).to.equal(0)
			expect(control.sockets).to.equal(0)
		end)
	end

	for _, boundary in { "status", "loading" } do
		it(`should finalize API closure during the initial {boundary} callback`, function()
			local session, control = fixture()
			if boundary == "status" then
				session:onStatusChanged(function(status)
					if status == ServeSession.Status.Connecting then
						session.__apiContext.__connected = false
					end
				end)
			else
				session.__updateLoadingText = function()
					session.__apiContext.__connected = false
				end
			end
			session:start()
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(#control.reads).to.equal(0)
			expect(control.sockets).to.equal(0)
		end)
	end

	it("should leave a replacement startup owned when its old read is cancelled", function()
		local session, control = fixture()
		session:start()
		control.replace()
		local replacement = session.__apiContext
		control.reads[1].promise:cancel()
		expect(session:getStatus()).to.equal(ServeSession.Status.Connecting)
		expect(replacement.__connected).to.equal(true)
		control.answer(2)
		expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
		expect(control.sockets).to.equal(1)
	end)

	it("should finalize owned startup work invalidated by API closure during a walk", function()
		local session, control = fixture()
		control.walk(function()
			session.__apiContext.__connected = false
		end)
		session:start()
		control.answer(1)
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(#control.applied).to.equal(0)
		expect(control.sockets).to.equal(0)
	end)

	for _, decision in { "Reject", "Accept" } do
		it(`should preserve direct initial startup sync with a nil token and {decision}`, function()
			local session, control = fixture()
			session.__userConfirmCallback = function()
				return decision
			end
			local result = session:__initialSync({ rootInstanceId = "ROOT" })
			control.answer(1)
			expect(result:await()).to.equal(true)
			expect(session.__startup).to.equal(nil)
			expect(#control.applied).to.equal(if decision == "Accept" then 1 else 0)
			expect(#control.writes).to.equal(if decision == "Reject" then 1 else 0)
			if decision == "Reject" then
				expect(control.writes[1].context).to.equal(session.__apiContext)
			end
		end)
	end

	it("should not clear replacement startup recovery when an old patch completes", function()
		local session, control = fixture()
		local completions = {}
		session.__applyPatch = function()
			return Promise.new(function(resolve)
				table.insert(completions, resolve)
			end)
		end
		session:start()
		control.answer(1)
		local oldRecovery = session.__initialSourceRecovery
		control.replace()
		control.answer(2)
		local newRecovery = session.__initialSourceRecovery
		expect(newRecovery ~= nil).to.equal(true)
		expect(newRecovery ~= oldRecovery).to.equal(true)
		completions[1]()
		expect(session.__initialSourceRecovery).to.equal(newRecovery)
		expect(control.sockets).to.equal(0)
		completions[2]()
		expect(session.__initialSourceRecovery).to.equal(nil)
		expect(control.sockets).to.equal(1)
	end)

	it("should not publish an old startup Source recovery checkpoint after replacement", function()
		local session, control = fixture()
		local answerCheckpoint
		local instance = Instance.new("ModuleScript")
		local draft: any = { Instance = instance, Revision = 0, Source = "draft" }
		session.__instanceMap.fromIds.SCRIPT = instance
		session.__instanceMap.fromInstances[instance] = "SCRIPT"
		session.__sourceDrafts = { SCRIPT = draft }
		session.__sourceRevisions = {}
		session.__sourceHistory = {
			classify = function()
				return "Independent"
			end,
		}
		session.__hasOpenScriptDocument = function()
			return false
		end
		session.__readEditorSource = function()
			return "draft"
		end
		local notified = false
		session.__notifySourceRecovery = function()
			notified = true
		end
		session.__apiContext.createSourceCheckpoint = function()
			return Promise.new(function(resolve)
				answerCheckpoint = resolve
			end)
		end
		session.__applyPatch = function(self, patch, cursor, isCurrent)
			return self:__checkpointDisplacedSources(patch, cursor, isCurrent)
		end
		session.__reconciler.diff = function()
			return true,
				{
					added = {},
					removed = {},
					updated = {
						{ id = "SCRIPT", changedProperties = { Source = { String = "disk" } } },
					},
				}
		end
		session:start()
		control.answer(1)
		control.replace()
		answerCheckpoint({ checkpointId = "obsolete" })
		expect(draft.CheckpointId).to.equal(nil)
		expect(notified).to.equal(false)
		expect(session:getStatus()).to.equal(ServeSession.Status.Connecting)
		session.__applyPatch = function() end
		control.answer(2)
		expect(control.sockets).to.equal(1)
		instance:Destroy()
	end)

	for _, decision in { "Reject", "Accept", "Abort" } do
		it(`should cancel an obsolete startup confirmation resumed with {decision}`, function()
			local session, control = fixture()
			local suspended
			session.__userConfirmCallback = function()
				suspended = coroutine.running()
				return coroutine.yield()
			end
			session:start()
			control.answer(1)
			expect(coroutine.status(suspended)).to.equal("suspended")
			control.replace()
			local resumed, problem = coroutine.resume(suspended, decision)
			assert(resumed, problem)
			expect(#control.writes).to.equal(0)
			expect(#control.applied).to.equal(0)
			expect(#control.hydrated).to.equal(1)
			expect(#control.cursors).to.equal(1)
			expect(session:getStatus()).to.equal(ServeSession.Status.Connecting)
			session.__userConfirmCallback = function()
				return "Accept"
			end
			control.answer(2)
			expect(#control.applied).to.equal(1)
			expect(control.sockets).to.equal(1)
			expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
		end)
	end

	it("should reject an obsolete startup read before mutating cursors or hydrating", function()
		local session, control = fixture()
		session:start()
		control.replace()
		control.answer(1)
		expect(#control.cursors).to.equal(0)
		expect(#control.hydrated).to.equal(0)
		control.answer(2)
		expect(#control.cursors).to.equal(1)
		expect(control.cursors[1].context).to.equal(session.__apiContext)
		expect(control.sockets).to.equal(1)
	end)

	for _, boundary in { "Hydrating instance map...", "Finding differences between server and Studio..." } do
		it(`should cancel startup reentered by loading text {boundary}`, function()
			local session, control = fixture()
			local replaced = false
			session.__updateLoadingText = function(text)
				if text == boundary and not replaced then
					replaced = true
					control.replace()
				end
			end
			session:start()
			control.answer(1)
			expect(#control.hydrated).to.equal(if boundary == "Hydrating instance map..." then 0 else 1)
			expect(#control.applied).to.equal(0)
			control.answer(2)
			expect(control.sockets).to.equal(1)
		end)
	end

	for _, walk in { "hydrate", "diff", "source" } do
		for _, action in { "stop", "replace", "close" } do
			it(`should cancel startup {walk} walk after {action} during a budget yield`, function()
				local session, control = fixture()
				local stepped = false
				control.walk(function()
					if stepped then
						return
					end
					stepped = true
					if action == "stop" then
						session:stop()
					elseif action == "close" then
						session.__apiContext:disconnect()
					else
						control.replace()
					end
				end)
				session.__reconciler.hydrate = function(_, instances, _, _, step)
					table.insert(control.hydrated, instances)
					if walk == "hydrate" then
						step()
					end
				end
				session.__reconciler.diff = function(_, _, _, step)
					if walk == "diff" then
						step()
					end
					return true, PatchSet.newEmpty()
				end
				session:start()
				control.reads[1].resolve({ instances = { SCRIPT = { Properties = {} } }, messageCursor = 1 })
				expect(stepped).to.equal(true)
				expect(#control.applied).to.equal(0)
				expect(#control.writes).to.equal(0)
				expect(control.sockets).to.equal(0)
				if action == "replace" then
					control.answer(2)
					expect(control.sockets).to.equal(1)
				end
			end)
		end
	end

	it("should cancel a delayed startup fallback before deserializing or reading refs", function()
		local session, control = fixture()
		local answerModels
		local refReads = 0
		session.__apiContext.serialize = function()
			return Promise.new(function(resolve)
				answerModels = resolve
			end)
		end
		session.__apiContext.refPatch = function()
			refReads += 1
			return Promise.resolve({ patch = PatchSet.newEmpty() })
		end
		session.__applyPatch = function(self, _, _, isCurrent)
			self:__replaceInstances({ "SCRIPT" }, function()
				if not isCurrent() then
					error(ServeSession.SYNC_CANCELLED, 0)
				end
			end)
		end
		session:start()
		control.answer(1)
		expect(type(answerModels)).to.equal("function")
		control.replace()
		answerModels({ modelContents = "Must not be deserialized" })
		expect(refReads).to.equal(0)
		expect(session:getStatus()).to.equal(ServeSession.Status.Connecting)
		session.__applyPatch = function() end
		control.answer(2)
		expect(control.sockets).to.equal(1)
	end)

	it("should complete replacement startup while an obsolete patch is still queued", function()
		local session, control = fixture()
		local resume
		session.__pendingSourcePatch = Promise.new(function(resolve)
			resume = resolve
		end)
		session.__applyPatch = ServeSession.__applyPatch
		session.__rememberSourcePublication = function() end
		session.__rememberStructuralPublication = function() end
		session.__filterSourceEchoes = function(_, patch)
			return patch
		end
		session.__applyPatchNow = function(_, patch)
			table.insert(control.applied, patch)
		end
		session:start()
		control.answer(1)
		control.replace()
		control.answer(2)
		expect(control.sockets).to.equal(1)
		expect(#control.applied).to.equal(1)
		resume()
		expect(control.sockets).to.equal(1)
		expect(#control.applied).to.equal(1)
		expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
	end)

	for _, boundary in { "queue", "precommit", "checkpoint" } do
		it(`should cancel delayed startup patch application at the {boundary} boundary`, function()
			local session, control = fixture()
			local resume
			local pending = Promise.new(function(resolve)
				resume = resolve
			end)
			session.__applyPatch = ServeSession.__applyPatch
			session.__rememberSourcePublication = function() end
			session.__rememberStructuralPublication = function() end
			session.__filterSourceEchoes = function(_, patch)
				return patch
			end
			session.__keepUndoneDeletions = function() end
			session.__protectLocalProperties = function(_, patch)
				return patch
			end
			session.__awaitSourceOperations = function() end
			session.__precommitCallbacks = {}
			session.__postcommitCallbacks = {}
			session.__reconciler.applyPatch = function(_, patch)
				table.insert(control.applied, patch)
				return PatchSet.newEmpty()
			end
			session.__checkpointDisplacedSources = function()
				if boundary == "checkpoint" then
					boundary = "checkpoint resumed"
					return pending
				end
				return nil
			end
			if boundary == "queue" then
				session.__pendingSourcePatch = pending
			elseif boundary == "precommit" then
				session.__precommitCallbacks = {
					function()
						control.replace()
					end,
				}
			end
			session:start()
			control.answer(1)
			if boundary ~= "precommit" then
				control.replace()
			end
			resume()
			expect(#control.applied).to.equal(0)
			expect(session:getStatus()).to.equal(ServeSession.Status.Connecting)
			-- The old finalizer must not clear the replacement's recovery state.
			session.__precommitCallbacks = {}
			session.__checkpointDisplacedSources = function() end
			control.answer(2)
			expect(#control.applied).to.equal(1)
			expect(control.sockets).to.equal(1)
		end)
	end

	it("should cancel startup replaced while encoding the inverse patch", function()
		local session, control = fixture()
		local replaced = false
		session.__instanceMap.fromIds.SCRIPT = setmetatable({}, {
			__index = function(_, key)
				if key == "Name" then
					if not replaced then
						replaced = true
						control.replace()
					end
					return "Old"
				end
				return nil
			end,
		})
		session.__reconciler.diff = function()
			return true,
				{
					added = {},
					removed = {},
					updated = { { id = "SCRIPT", changedName = "Disk", changedProperties = {} } },
				}
		end
		session.__userConfirmCallback = function()
			return "Reject"
		end
		session:start()
		control.answer(1)
		expect(replaced).to.equal(true)
		expect(#control.writes).to.equal(0)
		session.__userConfirmCallback = function()
			return "Accept"
		end
		control.answer(2)
		expect(control.sockets).to.equal(1)
	end)
end
