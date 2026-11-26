--[[

ServeSession.direction.spec

Drives real startup, publication, write and suspension control flow with deterministic
network and editor boundaries. No native Studio session is needed.

--]]

return function()
	const ServeSession = require("./ServeSession")
	const Policy = require("./DirectionPolicy")
	const Config = require("./Config")
	const Promise = require(script.Parent.Parent.Packages.Promise)
	const PatchSet = require("./PatchSet")
	const SourceHistory = require("./SourceHistory")
	local environment, originalWorkspace, attributes, sessions, instances

	const function api(control, info)
		const context = { __connected = true, cursor = 0 }
		function context:connect()
			return Promise.resolve(info)
		end
		function context:getBaseUrl()
			return control.endpoint
		end
		function context:getMessageCursor()
			return self.cursor
		end
		function context:setMessageCursor(cursor)
			self.cursor = cursor
		end
		function context:read()
			control.reads += 1
			return Promise.resolve({ instances = {}, messageCursor = 10 })
		end
		function context:disconnect()
			self.__connected = false
		end
		function context:connectWebSocket(handlers)
			control.handlers = handlers
			return Promise.new(function() end)
		end
		function context:write()
			control.writes += 1
			return control.writeResult or Promise.reject("No response.")
		end
		return context
	end

	const function fixture(direction)
		const control = { endpoint = "http://localhost:34872", reads = 0, hydrates = 0, applied = {}, writes = 0 }
		const info = {
			sessionId = "session",
			projectName = "Directional",
			rootInstanceId = "ROOT",
			syncDirection = direction,
			supportedSyncDirections = { "fs-to-studio", "studio-to-fs", "bidirectional" },
		}
		const map = {
			fromIds = {},
			fromInstances = {},
			knownParents = {},
			stop = function(self)
				self.stopped = true
			end,
			captureStructuralChanges = function() end,
		}
		const batcher = {
			__pendingPropertyChanges = {},
			__pendingAdditions = {},
			stop = function(self)
				self.stopped = true
			end,
			isEmpty = function(self)
				return next(self.__pendingPropertyChanges) == nil and next(self.__pendingAdditions) == nil
			end,
			add = function(self, instance, property)
				self.__pendingPropertyChanges[instance] = self.__pendingPropertyChanges[instance] or {}
				self.__pendingPropertyChanges[instance][property] = true
			end,
			addChild = function(self, instance)
				self.__pendingAdditions[instance] = true
			end,
		}
		const session = setmetatable({
			__apiContext = api(control, info),
			__twoWaySync = true,
			__configuredTwoWaySync = true,
			__liveEditMarker = Policy.marker("localhost:34872", "session", direction),
			__status = ServeSession.Status.NotStarted,
			__connections = {},
			__instanceMap = map,
			__changeBatcher = batcher,
			__sourceApplyBatches = {},
			__remoteSources = {},
			__writeTickets = {},
			__sourceBaselines = {},
			__sourcePendingCounts = {},
			__sourceRevisions = {},
			__sourceDrafts = {},
			__localPropertyEdits = {},
			__syncResults = {},
			__precommitCallbacks = {},
			__postcommitCallbacks = {},
			__sourcePublicationCursors = {},
			__structuralPublicationCursors = {},
			__sourceHistory = SourceHistory.new({ Clock = os.clock, Horizon = 10, MaxBytes = 1024, MaxEntries = 10 }),
			__updateLoadingText = function() end,
			__applyGameAndPlaceId = function() end,
			__releaseDeferredChanges = function() end,
			__captureStructuralChanges = function() end,
			__pruneStructuralGuards = function() end,
			__applyPatchNow = function(_, patch)
				table.insert(control.applied, patch)
			end,
			__reconciler = {
				hydrate = function()
					control.hydrates += 1
				end,
				diff = function()
					return true, control.initialPatch or PatchSet.newEmpty()
				end,
			},
		}, ServeSession)
		table.insert(sessions, session)
		return session, control, info
	end

	const function acceptedResult(updates, cursor)
		return {
			messageCursor = cursor,
			accepted = { removed_instances = {}, added_instances = {}, updated_instances = updates },
			conflicts = { removed_instances = {}, added_instances = {}, updated_instances = {} },
			errors = {},
		}
	end

	beforeEach(function()
		environment = getfenv(ServeSession.__negotiateDirection)
		originalWorkspace = environment.workspace
		attributes, sessions, instances = {}, {}, {}
		environment.workspace = {
			GetAttribute = function(_, key)
				return attributes[key]
			end,
			SetAttribute = function(_, key, value)
				attributes[key] = value
			end,
		}
	end)

	afterEach(function()
		for _, session in sessions do
			session:destroy()
		end
		for _, instance in instances do
			instance:Destroy()
		end
		environment.workspace = originalWorkspace
	end)

	for _, direction in { "fs-to-studio", "studio-to-fs", "bidirectional" } do
		it(`negotiates {direction} before the initial read and pins its write gate`, function()
			const session, control = fixture(direction)
			session:start()
			expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
			expect(session:getSyncDirection()).to.equal(direction)
			expect(session.__twoWaySync).to.equal(direction ~= "fs-to-studio")
			expect(session.__changeBatcher.__suspended).to.equal(direction == "fs-to-studio")
			expect(control.reads).to.equal(1)
		end)
	end

	for _, direction in { "fs-to-studio", "studio-to-fs", "bidirectional" } do
		it(`resumes exact instance identity and publication direction under {direction}`, function()
			const session, control, info = fixture(direction)
			session:start()
			const instance = Instance.new("Folder")
			instances = { instance }
			session.__instanceMap.fromIds.FOLDER = instance
			session.__instanceMap.fromInstances[instance] = "FOLDER"
			session:stop()
			expect(session:getResumeKey()).to.equal(Policy.resumeKey(control.endpoint, "session", direction))
			session:resume(api(control, info), session.__liveEditMarker)
			session:start()
			expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
			expect(session.__instanceMap.fromIds.FOLDER).to.equal(instance)
			expect(control.reads).to.equal(1)
			expect(control.hydrates).to.equal(1)
			control.handlers.messages({
				messages = { { origin = "filesystem", added = {}, removed = {}, updated = {} } },
				messageCursor = 11,
			})
			expect(session.__apiContext:getMessageCursor()).to.equal(11)
			expect(#control.applied).to.equal(if direction == "studio-to-fs" then 1 else 2)
		end)
	end

	it("permits initial filesystem normalization exactly once and never repeats it on reconnect", function()
		const session, control, info = fixture("studio-to-fs")
		const normalization = {
			added = {},
			removed = {},
			updated = {
				{ id = "SCRIPT", changedProperties = { Source = { String = "normalized imports" } } },
			},
		}
		control.initialPatch = normalization
		session:start()
		expect(control.applied[1]).to.equal(normalization)
		expect(attributes[Config.liveEditBootstrapAttribute]).to.equal("session")
		session:stop()
		session:resume(api(control, info), session.__liveEditMarker)
		session:start()
		expect(control.reads).to.equal(1)
		expect(#control.applied).to.equal(1)
	end)

	for _, direction in { "fs-to-studio", "studio-to-fs" } do
		it(`refuses inverse initial confirmation under {direction} without writes`, function()
			const session, control = fixture(direction)
			session:setConfirmCallback(function()
				return "Reject"
			end)
			session:start()
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(control.writes).to.equal(0)
			expect(#control.applied).to.equal(0)
		end)
	end

	it("keeps the consumed cursor across a failed reconnect before retrying the same identity", function()
		const session, control, info = fixture("studio-to-fs")
		session:start()
		session.__apiContext:setMessageCursor(17)
		session:stop()
		const failedApi = api(control, info)
		failedApi.connect = function()
			return Promise.reject("Offline.")
		end
		session:resume(failedApi, session.__liveEditMarker)
		session:start()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		session:resume(api(control, info), session.__liveEditMarker)
		session:start()
		expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
		expect(session.__apiContext:getMessageCursor()).to.equal(17)
		expect(control.reads).to.equal(1)
	end)

	it("rejects changed policy on reconnect before reseeding the place", function()
		const session, control, info = fixture("studio-to-fs")
		session:start()
		session:stop()
		info.syncDirection = "bidirectional"
		session:resume(api(control, info), session.__liveEditMarker)
		session:start()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(control.reads).to.equal(1)
		expect(control.hydrates).to.equal(1)
	end)

	it("refuses a reused port before reading or hydrating", function()
		const session, control, info = fixture("studio-to-fs")
		info.sessionId = "replacement"
		session:start()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(control.reads).to.equal(0)
		expect(control.hydrates).to.equal(0)
		expect(next(attributes)).to.equal(nil)
	end)

	it("refuses reopened bootstrapped places without in-memory identity evidence", function()
		attributes[Config.liveEditBootstrapAttribute] = "session"
		const session, control = fixture("studio-to-fs")
		session:start()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(control.reads).to.equal(0)
		expect(control.hydrates).to.equal(0)
	end)

	it("resumes the same references and queued intent without reseeding or name hydration", function()
		const session, control, info = fixture("studio-to-fs")
		session:start()
		const moved, deleted, editorOnly = Instance.new("ModuleScript"), Instance.new("Folder"), Instance.new("Folder")
		moved.Name = "AuthoredMove"
		moved.Parent = editorOnly
		deleted.Parent = nil
		instances = { moved, deleted, editorOnly }
		session.__instanceMap.fromIds.MOVED = moved
		session.__instanceMap.fromIds.DELETED = deleted
		session.__instanceMap.fromInstances[moved] = "MOVED"
		session.__instanceMap.fromInstances[deleted] = "DELETED"
		const changes = { [moved] = { Name = true, Parent = true, Source = true }, [deleted] = { Parent = true } }
		const additions = { [editorOnly] = true }
		session.__changeBatcher.__pendingPropertyChanges = changes
		session.__changeBatcher.__pendingAdditions = additions
		const baseline = { Cursor = 8, Revision = 2 }
		session.__sourceBaselines[moved] = baseline
		const draft = { Instance = moved, Source = "authored", Revision = 3 }
		session.__sourceDrafts.MOVED = draft
		const edits = session.__localPropertyEdits
		const head = { Promise = Promise.resolve({ "accepted-proof" }) }
		const dependencies = { head }
		session.__writeHead, session.__pendingWriteDependencies = head, dependencies
		session.__apiContext:setMessageCursor(15)
		session:stop()
		expect(session.__instanceMap.stopped).to.equal(nil)
		expect(session.__changeBatcher.stopped).to.equal(nil)
		expect(session.__changeBatcher.__suspended).to.equal(true)
		session:resume(api(control, info), session.__liveEditMarker)
		session:start()
		expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
		expect(control.reads).to.equal(1)
		expect(control.hydrates).to.equal(1)
		expect(session.__apiContext:getMessageCursor()).to.equal(15)
		expect(session.__instanceMap.fromIds.MOVED).to.equal(moved)
		expect(session.__instanceMap.fromIds.DELETED).to.equal(deleted)
		expect(session.__changeBatcher.__pendingPropertyChanges).to.equal(changes)
		expect(session.__changeBatcher.__pendingAdditions).to.equal(additions)
		expect(session.__sourceBaselines[moved]).to.equal(baseline)
		expect(session.__sourceDrafts.MOVED).to.equal(draft)
		expect(session.__localPropertyEdits).to.equal(edits)
		expect(session.__writeHead).to.equal(head)
		expect(session.__pendingWriteDependencies).to.equal(dependencies)
	end)

	it(
		"continues observing disconnected local structure additions and Source intent under the retained lease",
		function()
			const session, control, info = fixture("studio-to-fs")
			session:start()
			const scriptInstance, child = Instance.new("ModuleScript"), Instance.new("Folder")
			instances = { scriptInstance, child }
			session.__instanceMap.fromIds.SCRIPT = scriptInstance
			session.__instanceMap.fromInstances[scriptInstance] = "SCRIPT"
			session.__instanceMap.getSourceRevision = function()
				return 0, "lease"
			end
			const baseline = { Cursor = 10, Revision = 0 }
			session.__sourceBaselines[scriptInstance] = baseline
			session:stop()
			session:__onInstanceChanged(scriptInstance, "Name")
			session:__onInstanceChanged(scriptInstance, "Parent")
			session:__onChildAdded(child)
			session:__onInstanceChanged(
				scriptInstance,
				"Source",
				{ Generation = "lease", Revision = 1, Value = "authored offline" }
			)
			const changes = session.__changeBatcher.__pendingPropertyChanges[scriptInstance]
			expect(changes.Name and changes.Parent and changes.Source).to.equal(true)
			expect(session.__changeBatcher.__pendingAdditions[child]).to.equal(true)
			expect(session.__sourceDrafts.SCRIPT.Source).to.equal("authored offline")
			expect(session.__localPropertyEdits.SCRIPT.Source.sourceBaseline).to.equal(baseline)
			session:resume(api(control, info), session.__liveEditMarker)
			session:start()
			expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
			expect(session.__sourceDrafts.SCRIPT.Source).to.equal("authored offline")
			expect(session.__changeBatcher.__pendingPropertyChanges[scriptInstance]).to.equal(changes)
		end
	)

	for _, uncertain in { "ticket", "encoding", "source" } do
		it(`retains drafts and refuses automatic replay after ambiguous {uncertain}`, function()
			const session, control, info = fixture("studio-to-fs")
			session:start()
			const draft = { Source = "retain me" }
			session.__sourceDrafts.SCRIPT = draft
			if uncertain == "ticket" then
				session.__writeTickets[{}] = {}
			elseif uncertain == "encoding" then
				session.__changeBatcher.__flushing = true
			else
				session.__pendingSourcePatch = Promise.new(function() end)
			end
			session:stop()
			session:resume(api(control, info), session.__liveEditMarker)
			session:start()
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(control.reads).to.equal(1)
			expect(session.__sourceDrafts.SCRIPT).to.equal(draft)
		end)
	end

	it("does not accept a late in-flight acknowledgement after suspension", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		local answer
		control.writeResult = Promise.new(function(resolve)
			answer = resolve
		end)
		const draft = { Source = "not acknowledged" }
		session.__sourceDrafts.SCRIPT = draft
		const writing = session:__writePatch(PatchSet.newEmpty())
		expect(control.writes).to.equal(1)
		session:stop()
		answer({
			messageCursor = 10,
			accepted = { added_instances = {}, removed_instances = {}, updated_instances = {} },
			conflicts = { added_instances = {}, removed_instances = {}, updated_instances = {} },
			errors = {},
		})
		writing:await()
		expect(session:getSyncResults()[1].status).to.equal("Failed")
		expect(session.__sourceDrafts.SCRIPT).to.equal(draft)
		expect(session.__resumeBlocked).to.equal(true)
	end)

	it("refuses resume after an unreadable offline Source observation without reseeding", function()
		const session, control, info = fixture("studio-to-fs")
		session:start()
		session.__instanceMap.getSourceRevision = function()
			return 0, "lease"
		end
		session:stop()
		session:__onInstanceChanged({}, "Source", { Generation = "lease", Error = "Editor unavailable." })
		expect(session.__resumeBlocked).to.equal(true)
		session:resume(api(control, info), session.__liveEditMarker)
		session:start()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(control.reads).to.equal(1)
	end)

	it("refuses resume when another observer replaced a retained Source generation", function()
		const session, control, info = fixture("studio-to-fs")
		session:start()
		session:stop()
		const instance = {}
		session.__instanceMap.sourceConnections = { [instance] = { Generation = "old" } }
		session.__instanceMap.getSourceRevision = function()
			return nil
		end
		session:resume(api(control, info), session.__liveEditMarker)
		session:start()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(control.reads).to.equal(1)
	end)

	it("advances the cursor over ignored disk changes without consuming Source baselines", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		const instance = {}
		session.__instanceMap.fromIds.SCRIPT = instance
		const baseline = { Cursor = 10, Revision = 0 }
		session.__sourceBaselines[instance] = baseline
		const disk = {
			origin = "filesystem",
			added = { NEW = {} },
			removed = { "SCRIPT" },
			updated = { { id = "SCRIPT", changedName = "Disk", changedProperties = { Source = { String = "disk" } } } },
		}
		control.handlers.messages({ messages = { disk }, messageCursor = 11 })
		expect(session.__apiContext:getMessageCursor()).to.equal(11)
		expect(#control.applied).to.equal(1)
		expect(session.__sourceBaselines[instance]).to.equal(baseline)
		expect(session.__sourcePublicationCursors[instance]).to.equal(nil)
		expect(session.__structuralPublicationCursors[instance]).to.equal(nil)
		expect(session.__instanceMap.fromIds.SCRIPT).to.equal(instance)
		const accepted = table.clone(disk)
		accepted.origin = "studio"
		control.handlers.messages({ messages = { accepted }, messageCursor = 12 })
		expect(#control.applied).to.equal(2)
		expect(control.applied[2].added).to.equal(accepted.added)
		expect(control.applied[2].removed).to.equal(accepted.removed)
		expect(session.__sourcePublicationCursors[instance]).to.equal(12)
		expect(session.__apiContext:getMessageCursor()).to.equal(12)
	end)

	it("fails closed on missing provenance before baselines or editor mutation", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		control.handlers.messages({ messages = { PatchSet.newEmpty() }, messageCursor = 11 })
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(session.__apiContext:getMessageCursor()).to.equal(10)
		expect(#control.applied).to.equal(1)
	end)

	for _, deferred in { false, true } do
		it(`retains drafts and refuses replay after normalization fails with deferred {deferred}`, function()
			const session, control, info = fixture("studio-to-fs")
			session:start()
			const draft = { Source = "retain me" }
			session.__sourceDrafts.SCRIPT = draft
			session.__applyPatchNow = function()
				if deferred then
					return Promise.reject("Unapplied normalization.")
				end
				error("Unapplied normalization.")
			end
			const patch = PatchSet.newEmpty()
			patch.origin = "studio"
			expect(session:__applyPatch(patch, 11):await()).to.equal(false)
			expect(session.__resumeBlocked).to.equal(true)
			expect(session.__sourceDrafts.SCRIPT).to.equal(draft)
			session:resume(api(control, info), session.__liveEditMarker)
			session:start()
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(control.reads).to.equal(1)
		end)
	end

	it("applies filesystem publications without two-way history bookkeeping", function()
		const session, control = fixture("fs-to-studio")
		session:start()
		const function unused()
			error("Filesystem-only sync must not visit two-way history.")
		end
		session.__rememberSourcePublication = unused
		session.__rememberStructuralPublication = unused
		session.__releaseLocalProperties = unused
		session.__sourceHistory.consume = unused
		control.handlers.messages({
			messages = { { origin = "filesystem", added = {}, removed = {}, updated = {} } },
			messageCursor = 11,
		})
		expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
		expect(session.__apiContext:getMessageCursor()).to.equal(11)
		expect(#control.applied).to.equal(2)
	end)

	it("skips the hydration-wide syncback baseline walk in filesystem-only mode", function()
		const session, control = fixture("fs-to-studio")
		const instance = Instance.new("ModuleScript")
		instances = { instance }
		session.__instanceMap.fromIds.SCRIPT = instance
		session.__instanceMap.fromInstances[instance] = "SCRIPT"
		session.__readEditorSource = function()
			error("Filesystem-only hydration must not capture syncback baselines.")
		end
		session.__apiContext.read = function()
			control.reads += 1
			return Promise.resolve({
				instances = { SCRIPT = { Properties = { Source = { String = "seed" } } } },
				messageCursor = 10,
			})
		end
		session:start()
		expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
		expect(next(session.__sourceBaselines)).to.equal(nil)
	end)

	it("does not scan structural intent in filesystem-only reconciliation", function()
		const session = fixture("fs-to-studio")
		session:start()
		session.__captureStructuralChanges = function()
			error("Filesystem-only sync has no captured structural write intent.")
		end
		const patch = PatchSet.newEmpty()
		expect(session:__protectLocalProperties(patch, 11)).to.equal(patch)
	end)

	it("preserves differing open startup drafts without requesting a filesystem-only checkpoint", function()
		const session, control = fixture("fs-to-studio")
		session:start()
		const instance = Instance.new("ModuleScript")
		instances = { instance }
		session.__instanceMap.fromIds.SCRIPT = instance
		session.__instanceMap.fromInstances[instance] = "SCRIPT"
		session.__initialSourceRecovery = {}
		session.__hasOpenScriptDocument = function()
			return true
		end
		session.__readEditorSource = function()
			return "unsaved draft"
		end
		local requested = false
		session.__apiContext.createSourceCheckpoint = function()
			requested = true
			return Promise.reject("Forbidden checkpoint.")
		end
		const patch = {
			added = {},
			removed = {},
			updated = { { id = "SCRIPT", changedProperties = { Source = { String = "disk" } } } },
		}
		const success, problem = session:__checkpointDisplacedSources(patch, 10):await()
		expect(success).to.equal(false)
		expect(tostring(problem):find("Save", 1, true) ~= nil).to.equal(true)
		expect(requested).to.equal(false)
		expect(control.writes).to.equal(0)
	end)

	it("skips filesystem-only recovery scanning outside initial normalization", function()
		const session = fixture("fs-to-studio")
		session:start()
		session.__instanceMap.fromIds.SCRIPT = Instance.new("ModuleScript")
		instances = { session.__instanceMap.fromIds.SCRIPT }
		session.__sourceHistory.classify = function()
			error("Filesystem-only publications do not classify syncback history.")
		end
		expect(session:__checkpointDisplacedSources({
			added = {},
			removed = {},
			updated = { { id = "SCRIPT", changedProperties = { Source = { String = "disk" } } } },
		}, 11)).to.equal(nil)
	end)

	for _, kind in { "removal", "ancestor removal", "class change" } do
		it(`preserves an open filesystem-only startup document before {kind}`, function()
			const session = fixture("fs-to-studio")
			session:start()
			const parent = Instance.new("Folder")
			const instance = Instance.new("ModuleScript")
			instance.Parent = parent
			instances = { instance, parent }
			session.__instanceMap.fromIds.SCRIPT = instance
			session.__instanceMap.fromIds.PARENT = parent
			session.__initialSourceRecovery = {}
			session.__hasOpenScriptDocument = function()
				return true
			end
			session.__readEditorSource = function()
				return "retained draft"
			end
			session.__apiContext.createSourceCheckpoint = function()
				error("Filesystem-only startup cannot create a checkpoint.")
			end
			const patch = PatchSet.newEmpty()
			if kind == "class change" then
				patch.updated = { { id = "SCRIPT", changedClassName = "Script", changedProperties = {} } }
			else
				patch.removed = { if kind == "removal" then "SCRIPT" else "PARENT" }
			end
			const success, problem = session:__checkpointDisplacedSources(patch, 10):await()
			expect(success).to.equal(false)
			expect(tostring(problem):find("Save", 1, true) ~= nil).to.equal(true)
			expect(instance.Parent).to.equal(parent)
		end)
	end

	it("allows matching open documents and closed documents without filesystem-only checkpoints", function()
		const session = fixture("fs-to-studio")
		session:start()
		const instance = Instance.new("ModuleScript")
		instances = { instance }
		session.__instanceMap.fromIds.SCRIPT = instance
		session.__initialSourceRecovery = {}
		local open = true
		session.__hasOpenScriptDocument = function()
			return open
		end
		session.__readEditorSource = function()
			return "same source"
		end
		const patch = {
			added = {},
			removed = {},
			updated = { { id = "SCRIPT", changedProperties = { Source = { String = "same source" } } } },
		}
		expect(session:__checkpointDisplacedSources(patch, 10)).to.equal(nil)
		open = false
		patch.updated[1].changedProperties.Source.String = "filesystem update"
		expect(session:__checkpointDisplacedSources(patch, 10)).to.equal(nil)
	end)

	for _, status in { "committed", "superseded", "failed", "cancelled" } do
		it(`awaits filesystem-only Source outcome {status} without syncback baselines`, function()
			const session = fixture("fs-to-studio")
			session:start()
			const instance = Instance.new("ModuleScript")
			instances = { instance }
			session.__instanceMap.fromIds.SCRIPT = instance
			session.__instanceMap.fromInstances[instance] = "SCRIPT"
			const function unused()
				error("Filesystem-only Source outcomes must not visit syncback history or baselines.")
			end
			session.__sourceRevision = unused
			session.__readEditorSource = unused
			session.__sourceHistory.classify = unused
			const operations = { Cursors = {} }
			session.__sourceApplyBatches[coroutine.running()] = operations
			const operation = { Value = "filesystem source" }
			session:__onSourceScheduled(instance, operation)
			expect(#operations).to.equal(1)
			const awaiting = session:__awaitSourceOperations(operations)
			operation.Status = status
			session:__onSourceOutcome(instance, operation)
			expect(awaiting:await()).to.equal(status == "committed" or status == "superseded")
			expect(next(session.__sourceBaselines)).to.equal(nil)
			expect(next(session.__remoteSources)).to.equal(nil)
			expect(next(session.__sourcePendingCounts)).to.equal(nil)
		end)
	end

	for _, direction in { "studio-to-fs", "bidirectional" } do
		it(`uses subscription progress without a frozen write cursor under {direction}`, function()
			const session, control = fixture(direction)
			session:start()
			const instance = Instance.new("ModuleScript")
			instances = { instance }
			session.__instanceMap.fromIds.SCRIPT = instance
			session.__instanceMap.fromInstances[instance] = "SCRIPT"
			-- An independent disk rename that Studio-only sync withholds.
			control.handlers.messages({
				messages = {
					{
						origin = "filesystem",
						added = {},
						removed = {},
						updated = { { id = "SCRIPT", changedName = "Disk", changedProperties = {} } },
					},
				},
				messageCursor = 11,
			})
			expect(session.__apiContext:getMessageCursor()).to.equal(11)
			const expected = 11
			session:__markBatchStart()
			expect(session.__pendingWriteCursor).to.equal(expected)
			session:__rememberLocalProperty(instance, "Source", { Value = "draft" })
			expect(session.__localPropertyEdits.SCRIPT.Source.baselineCursor).to.equal(expected)
			-- Studio authority needs no causal cap after a withheld disk publication.
			control.handlers.messages({
				messages = { { origin = "studio", added = {}, removed = {}, updated = {} } },
				messageCursor = 12,
			})
			expect(session.__apiContext:getMessageCursor()).to.equal(12)
			const afterGap = 12
			session.__changeBatcher.__pendingPropertyChanges = {}
			session.__changeBatcher.__pendingAdditions = {}
			session:__markBatchStart()
			expect(session.__pendingWriteCursor).to.equal(afterGap)
			local sentCursor
			session.__apiContext.write = function(_, _patch, cursor)
				sentCursor = cursor
				return Promise.new(function() end)
			end
			session:__writePatch({
				added = {},
				removed = {},
				updated = { { id = "SCRIPT", changedName = "Local", changedProperties = {} } },
			})
			expect(sentCursor).to.equal(afterGap)
		end)
	end

	it("retains subscription progress across a same-session resume without frozen write evidence", function()
		const session, control, info = fixture("studio-to-fs")
		session:start()
		control.handlers.messages({
			messages = { { origin = "filesystem", added = {}, removed = {}, updated = {} } },
			messageCursor = 11,
		})
		session:stop()
		session:resume(api(control, info), session.__liveEditMarker)
		session:start()
		expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
		expect(session.__apiContext:getMessageCursor()).to.equal(11)
		expect(session.__apiContext:getMessageCursor()).to.equal(11)
	end)

	for _, direction in { "fs-to-studio", "studio-to-fs", "bidirectional" } do
		it(`does not reconcile a publication after disconnect during a precommit hook under {direction}`, function()
			const session, control = fixture(direction)
			session:start()
			session.__applyPatchNow = nil
			const folder = Instance.new("Folder")
			instances = { folder }
			session.__instanceMap.fromIds.FOLDER = folder
			session.__instanceMap.fromInstances[folder] = "FOLDER"
			local mutated = false
			session.__reconciler.applyPatch = function()
				mutated = true
				return PatchSet.newEmpty()
			end
			table.insert(session.__precommitCallbacks, function()
				-- Models a hook that yielded while the user disconnected.
				session:stop()
			end)
			control.handlers.messages({
				messages = {
					{
						origin = if direction == "studio-to-fs" then "studio" else "filesystem",
						added = {},
						removed = {},
						updated = { { id = "FOLDER", changedName = "Renamed", changedProperties = {} } },
					},
				},
				messageCursor = 11,
			})
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(mutated).to.equal(false)
		end)
	end

	it("enforces Studio-only provenance when a subscription supplies an ownership predicate", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		const current = function()
			return true
		end
		const applied = #control.applied
		expect(
			session:__applyPatch({ origin = "filesystem", added = {}, removed = {}, updated = {} }, 11, current):await()
		).to.equal(true)
		expect(#control.applied).to.equal(applied)
		expect(session.__apiContext:getMessageCursor()).to.equal(10)
		expect(session:__applyPatch(PatchSet.newEmpty(), 12, current):await()).to.equal(false)
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
	end)

	it("still applies the one initial filesystem normalization with an ownership predicate", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		const applied = #control.applied
		const current = function()
			return true
		end
		session:__applyPatch({ origin = "filesystem", added = {}, removed = {}, updated = {} }, 10, current, true)
		expect(#control.applied).to.equal(applied + 1)
	end)

	for _, direction in { "studio-to-fs", "bidirectional" } do
		for _, configured in { "Filesystem", "Studio" } do
			it(`chooses live write authority for {direction} with configured {configured}`, function()
				const session = fixture(direction)
				session.__twoWaySyncAuthority = configured
				session:start()
				local sent
				session.__apiContext.write = function(_, _patch, _cursor, authority)
					sent = authority
					return Promise.resolve(acceptedResult({}, 10))
				end
				session:__writePatch(PatchSet.newEmpty()):await()
				expect(sent).to.equal(if direction == "studio-to-fs" then "Studio" else configured)
				expect(session:getSyncResults()[1].status).to.equal("Saved")
			end)
		end
	end

	it("reports one visible notice without changing an accepted write from Saved", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		const instance = Instance.new("Folder")
		instance.Name = "Retained"
		instances = { instance }
		session.__instanceMap.fromIds.TARGET = instance
		session.__instanceMap.fromInstances[instance] = "TARGET"
		control.handlers.messages({
			messages = {
				{
					origin = "filesystem",
					added = {},
					removed = {},
					updated = {
						{ id = "TARGET", changedName = "Disk", changedProperties = {} },
					},
				},
			},
			messageCursor = 11,
		})
		control.writeResult =
			Promise.resolve(acceptedResult({ { id = "TARGET", changed_name = "Studio", changed_properties = {} } }, 12))
		local notified
		session:setSyncResultCallback(function(status, result)
			if status == "Saved" then
				notified = result.notices
			end
		end)
		const writing = session:__writePatch({
			added = {},
			removed = {},
			updated = { { id = "TARGET", changedName = "Studio", changedProperties = {} } },
		})
		control.handlers.messages({
			messages = { { origin = "studio", added = {}, removed = {}, updated = {} } },
			messageCursor = 12,
		})
		writing:await()
		expect(session:getSyncResults()[1].status).to.equal("Saved")
		expect(#session:getSyncResults()).to.equal(1)
		expect(#notified).to.equal(1)
		expect(notified[1].message).to.equal('Studio replaced a filesystem change to "Retained".')
		expect(next(session.__withheldFilesystemTargets)).to.equal(nil)
		expect(session:__replacedFilesystemNotices(acceptedResult({ { id = "TARGET", changed_name = "Again" } }, 13))).to.equal(
			nil
		)
	end)

	it("retains unrelated fields and newer disk publications when reporting overwrites", function()
		const session = fixture("studio-to-fs")
		session:start()
		const instance = Instance.new("Folder")
		instances = { instance }
		session.__instanceMap.fromIds.TARGET = instance
		const packet = {
			origin = "filesystem",
			added = {},
			removed = {},
			updated = {
				{ id = "TARGET", changedName = "Disk", changedProperties = { Size = { Vector3 = { 1, 2, 3 } } } },
			},
		}
		session:__withholdPublication(packet, 11)
		expect(session:__replacedFilesystemNotices(acceptedResult({ { id = "OTHER", changed_name = "Other" } }, 12))).to.equal(
			nil
		)
		expect(
			session:__replacedFilesystemNotices(
				acceptedResult({ { id = "TARGET", changed_properties = { Source = { String = "independent" } } } }, 12)
			)
		).to.equal(nil)
		const notice =
			session:__replacedFilesystemNotices(acceptedResult({ { id = "TARGET", changed_name = "Studio" } }, 12))
		expect(#notice).to.equal(1)
		expect(session.__withheldFilesystemTargets.TARGET).never.to.equal(nil)
		packet.updated[1].changedName = "Newer disk"
		session:__withholdPublication(packet, 14)
		expect(
			session:__replacedFilesystemNotices(acceptedResult({ { id = "TARGET", changed_name = "Older write" } }, 13))
		).to.equal(nil)
		expect(session.__withheldFilesystemTargets.TARGET).never.to.equal(nil)
	end)

	it("keeps overwrite notices absent for failed or conflicted writes", function()
		const session = fixture("studio-to-fs")
		session:start()
		const instance = Instance.new("Folder")
		instances = { instance }
		session.__instanceMap.fromIds.TARGET = instance
		session:__withholdPublication({
			origin = "filesystem",
			added = {},
			removed = {},
			updated = {
				{ id = "TARGET", changedName = "Disk", changedProperties = {} },
			},
		}, 11)
		for _, outcome in { "failed", "conflicted" } do
			const result = acceptedResult({ { id = "TARGET", changed_name = "Partial" } }, 12)
			if outcome == "failed" then
				result.errors = { { id = "TARGET", message = "Ownership changed." } }
			else
				result.conflicts.updated_instances = { { id = "TARGET", changed_name = "Disk" } }
			end
			expect(session:__replacedFilesystemNotices(result)).to.equal(nil)
			expect(session.__withheldFilesystemTargets.TARGET).never.to.equal(nil)
		end
	end)

	it("retains overwrite evidence through resume and emits one plural notice", function()
		const session, control, info = fixture("studio-to-fs")
		session:start()
		const updates = {}
		const accepted = {}
		for _, id in { "ONE", "TWO", "THREE" } do
			const instance = Instance.new("Folder")
			table.insert(instances, instance)
			session.__instanceMap.fromIds[id] = instance
			table.insert(updates, { id = id, changedName = "Disk", changedProperties = {} })
			table.insert(accepted, { id = id, changed_name = "Studio", changed_properties = {} })
		end
		session:__withholdPublication({ origin = "filesystem", added = {}, removed = {}, updated = updates }, 11)
		session:stop()
		session:resume(api(control, info), session.__liveEditMarker)
		session:start()
		const notices = session:__replacedFilesystemNotices(acceptedResult(accepted, 12))
		expect(#notices).to.equal(1)
		expect(notices[1].message).to.equal("Studio replaced filesystem changes to 3 instances.")
		expect(next(session.__withheldFilesystemTargets)).to.equal(nil)
	end)

	for _, direction in { "fs-to-studio", "bidirectional" } do
		it(`does not track overwrite notices under {direction}`, function()
			const session = fixture(direction)
			session:start()
			session:__withholdPublication({ origin = "filesystem", added = {}, removed = {}, updated = {} }, 11)
			expect(session.__withheldFilesystemTargets).to.equal(nil)
			expect(session.__noticePublicationCursor).to.equal(nil)
			expect(session.__noticePublicationWaiters).to.equal(nil)
			expect(session:__replacedFilesystemNotices(acceptedResult({}, 12))).to.equal(nil)
		end)
	end

	it("forgets overwrite evidence when the mapped instance identity changes", function()
		const session = fixture("studio-to-fs")
		session:start()
		const first, second = Instance.new("Folder"), Instance.new("Folder")
		instances = { first, second }
		session.__instanceMap.fromIds.TARGET = first
		session:__withholdPublication({
			origin = "filesystem",
			added = {},
			removed = {},
			updated = {
				{ id = "TARGET", changedName = "Disk", changedProperties = {} },
			},
		}, 11)
		session.__instanceMap.fromIds.TARGET = second
		expect(
			session:__replacedFilesystemNotices(
				acceptedResult({ { id = "TARGET", changed_name = "New identity" } }, 12)
			)
		).to.equal(nil)
		expect(next(session.__withheldFilesystemTargets)).to.equal(nil)
	end)

	for _, property in { "Source", "Parent", "ClassName", "Size" } do
		it(`matches accepted wire fields when reporting a replaced filesystem {property}`, function()
			const session = fixture("studio-to-fs")
			session:start()
			const instance = Instance.new("Folder")
			instances = { instance }
			session.__instanceMap.fromIds.TARGET = instance
			const disk = { id = "TARGET", changedProperties = {} }
			const accepted = { id = "TARGET", changed_properties = {} }
			if property == "Parent" then
				disk.changedParent = "DISK_PARENT"
				accepted.changedParent = "STUDIO_PARENT"
			elseif property == "ClassName" then
				disk.changedClassName = "Model"
				accepted.changed_class_name = "Folder"
			else
				disk.changedProperties[property] = { String = "disk" }
				accepted.changed_properties[property] = { String = "studio" }
			end
			session:__withholdPublication({ origin = "filesystem", added = {}, removed = {}, updated = { disk } }, 11)
			const notices = session:__replacedFilesystemNotices(acceptedResult({ accepted }, 12))
			expect(#notices).to.equal(1)
			expect(next(session.__withheldFilesystemTargets)).to.equal(nil)
		end)
	end

	it("reports an accepted deletion over withheld filesystem changes before untracking its target", function()
		const session = fixture("studio-to-fs")
		session:start()
		const instance = Instance.new("Folder")
		instances = { instance }
		session.__instanceMap.fromIds.TARGET = instance
		session:__withholdPublication({ origin = "filesystem", added = {}, removed = { "TARGET" }, updated = {} }, 11)
		const result = acceptedResult({}, 12)
		result.accepted.removed_instances = { "TARGET" }
		expect(#session:__replacedFilesystemNotices(result)).to.equal(1)
		expect(next(session.__withheldFilesystemTargets)).to.equal(nil)
	end)

	for _, acknowledgementFirst in { true, false } do
		it(
			`correlates overwrite notices with the original save when acknowledgement first is {acknowledgementFirst}`,
			function()
				const session, control = fixture("studio-to-fs")
				session:start()
				const instance = Instance.new("Folder")
				instance.Name = "Retained"
				instances = { instance }
				session.__instanceMap.fromIds.TARGET = instance
				session.__instanceMap.fromInstances[instance] = "TARGET"
				local answer
				control.writeResult = Promise.new(function(resolve)
					answer = resolve
				end)
				local saved = 0
				session:setSyncResultCallback(function(status)
					if status == "Saved" then
						saved += 1
					end
				end)
				const patch = {
					added = {},
					removed = {},
					updated = { { id = "TARGET", changedName = "Studio", changedProperties = {} } },
				}
				const writing = session:__writePatch(patch)
				const result =
					acceptedResult({ { id = "TARGET", changed_name = "Studio", changed_properties = {} } }, 12)
				if acknowledgementFirst then
					answer(result)
					expect(session:getSyncResults()[1].status).to.equal("Pending")
					expect(saved).to.equal(0)
				end
				control.handlers.messages({
					messages = {
						{
							origin = "filesystem",
							added = {},
							removed = {},
							updated = { { id = "TARGET", changedName = "Disk", changedProperties = {} } },
						},
						{ origin = "studio", added = {}, removed = {}, updated = {} },
					},
					messageCursor = 12,
				})
				if not acknowledgementFirst then
					answer(result)
				end
				expect(writing:await()).to.equal(true)
				expect(saved).to.equal(1)
				expect(#session:getSyncResults()).to.equal(1)
				expect(#session:getSyncResults()[1].result.notices).to.equal(1)
				expect(next(session.__withheldFilesystemTargets)).to.equal(nil)
				control.writeResult = Promise.resolve(
					acceptedResult({ { id = "TARGET", changed_name = "Again", changed_properties = {} } }, 13)
				)
				const again = session:__writePatch(patch)
				control.handlers.messages({
					messages = { { origin = "studio", added = {}, removed = {}, updated = {} } },
					messageCursor = 13,
				})
				expect(again:await()).to.equal(true)
				expect(saved).to.equal(2)
				expect(session:getSyncResults()[2].result.notices).to.equal(nil)
				expect(session.__noticePublicationWaiters).to.equal(nil)
			end
		)
	end

	it("does not wait for own reconciliation to correlate Saved notices and sequential writes", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		local apply
		session.__applyPatchNow = function()
			return Promise.new(function(resolve)
				apply = resolve
			end)
		end
		control.writeResult = Promise.resolve(acceptedResult({}, 12))
		const first = session:__writePatch(PatchSet.newEmpty())
		control.writeResult = Promise.resolve(acceptedResult({}, 13))
		const second = session:__writePatch(PatchSet.newEmpty())
		expect(control.writes).to.equal(1)
		control.handlers.messages({
			messages = { { origin = "studio", added = {}, removed = {}, updated = {} } },
			messageCursor = 12,
		})
		expect(first:getStatus()).to.equal(Promise.Status.Resolved)
		expect(control.writes).to.equal(2)
		expect(session.__apiContext:getMessageCursor()).to.equal(10)
		control.handlers.messages({
			messages = { { origin = "studio", added = {}, removed = {}, updated = {} } },
			messageCursor = 13,
		})
		expect(second:getStatus()).to.equal(Promise.Status.Resolved)
		expect(session.__noticePublicationWaiters).to.equal(nil)
		apply()
	end)

	it("keeps later filesystem evidence when one packet releases an older acknowledgement", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		const instance = Instance.new("Folder")
		instances = { instance }
		session.__instanceMap.fromIds.TARGET = instance
		session.__instanceMap.fromInstances[instance] = "TARGET"
		control.writeResult =
			Promise.resolve(acceptedResult({ { id = "TARGET", changed_name = "Studio", changed_properties = {} } }, 12))
		const writing = session:__writePatch({
			added = {},
			removed = {},
			updated = { { id = "TARGET", changedName = "Studio", changedProperties = {} } },
		})
		control.handlers.messages({
			messages = {
				{
					origin = "filesystem",
					added = {},
					removed = {},
					updated = {
						{
							id = "TARGET",
							changedName = "Disk",
							changedProperties = { Size = { String = "unrelated" } },
						},
					},
				},
				{ origin = "studio", added = {}, removed = {}, updated = {} },
				{
					origin = "filesystem",
					added = {},
					removed = {},
					updated = { { id = "TARGET", changedName = "Newer disk", changedProperties = {} } },
				},
			},
			messageCursor = 13,
		})
		expect(writing:await()).to.equal(true)
		expect(#session:getSyncResults()[1].result.notices).to.equal(1)
		expect(session.__withheldFilesystemTargets.TARGET.Fields.Name).to.equal(13)
		expect(session.__withheldFilesystemTargets.TARGET.Fields.Size).to.equal(11)
		expect(session:__replacedFilesystemNotices(acceptedResult({ { id = "TARGET", changed_name = "Older" } }, 12))).to.equal(
			nil
		)
	end)

	it("rejects notice waits on disconnect and never reports a late Saved", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		control.writeResult = Promise.resolve(acceptedResult({}, 12))
		local saved = 0
		session:setSyncResultCallback(function(status)
			if status == "Saved" then
				saved += 1
			end
		end)
		const writing = session:__writePatch(PatchSet.newEmpty())
		expect(session:getSyncResults()[1].status).to.equal("Pending")
		session:stop()
		control.handlers.messages({
			messages = { { origin = "studio", added = {}, removed = {}, updated = {} } },
			messageCursor = 12,
		})
		writing:await()
		expect(saved).to.equal(0)
		expect(session:getSyncResults()[1].status).to.equal("Failed")
		expect(session.__noticePublicationWaiters).to.equal(nil)
		expect(next(session.__writeTickets)).to.equal(nil)
	end)

	it("removes cancelled publication waiters without advancing receipt", function()
		const session = fixture("studio-to-fs")
		session:start()
		const waiting = session:__awaitNoticePublications(12)
		waiting:cancel()
		expect(session.__noticePublicationWaiters).to.equal(nil)
		expect(session.__noticePublicationCursor).to.equal(10)
	end)

	it("validates a whole publication packet before releasing notice waits", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		control.writeResult = Promise.resolve(acceptedResult({}, 11))
		const writing = session:__writePatch(PatchSet.newEmpty())
		control.handlers.messages({
			messages = { { origin = "studio", added = {}, removed = {}, updated = {} }, PatchSet.newEmpty() },
			messageCursor = 12,
		})
		writing:await()
		expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		expect(session:getSyncResults()[1].status).to.equal("Failed")
		expect(session.__noticePublicationWaiters).to.equal(nil)
	end)

	it("reports acknowledged removals after late filesystem receipt and before untracking", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		const instance = Instance.new("Folder")
		instance.Name = "Deleted"
		instances = { instance }
		session.__instanceMap.fromIds.TARGET = instance
		session.__instanceMap.fromInstances[instance] = "TARGET"
		session.__instanceMap.removeId = function(_, id)
			session.__instanceMap.fromIds[id] = nil
		end
		const result = acceptedResult({}, 12)
		result.accepted.removed_instances = { "TARGET" }
		control.writeResult = Promise.resolve(result)
		const writing = session:__writePatch({ added = {}, removed = { "TARGET" }, updated = {} })
		expect(session.__instanceMap.fromIds.TARGET).to.equal(instance)
		control.handlers.messages({
			messages = {
				{ origin = "filesystem", added = {}, removed = { "TARGET" }, updated = {} },
				{ origin = "studio", added = {}, removed = {}, updated = {} },
			},
			messageCursor = 12,
		})
		expect(writing:await()).to.equal(true)
		expect(session:getSyncResults()[1].result.notices[1].message).to.equal(
			'Studio replaced a filesystem change to "Deleted".'
		)
		expect(session.__instanceMap.fromIds.TARGET).to.equal(nil)
	end)

	it("does not gate failed Studio-only saves or other modes on publication receipt", function()
		for _, direction in { "studio-to-fs", "bidirectional" } do
			attributes = {}
			const session, control = fixture(direction)
			session:start()
			const result = acceptedResult({}, 200)
			if direction == "studio-to-fs" then
				result.errors = { { message = "Ownership changed." } }
			end
			control.writeResult = Promise.resolve(result)
			expect(session:__writePatch(PatchSet.newEmpty()):getStatus()).to.equal(Promise.Status.Resolved)
			expect(session:getSyncResults()[1].status).to.equal(
				if direction == "studio-to-fs" then "Failed" else "Saved"
			)
			expect(session.__noticePublicationWaiters).to.equal(nil)
		end
	end)

	it("blocks direct session writes and checkpoint restore in filesystem-only mode", function()
		const session, control = fixture("fs-to-studio")
		session:start()
		expect(session:__writePatch(PatchSet.newEmpty()):await()).to.equal(false)
		expect(session:restoreSourceCheckpoint("checkpoint", "SCRIPT", "draft"):await()).to.equal(false)
		expect(control.writes).to.equal(0)
	end)

	it("never replaces failed Studio-only normalization with a fresh filesystem snapshot", function()
		const Settings = require("./Settings")
		const originalGet = Settings.get
		Settings.get = function(_, key)
			return if key == "enableSyncFallback" then true else originalGet(Settings, key)
		end
		const success, problem = pcall(function()
			const session = fixture("studio-to-fs")
			session:start()
			const draft = { Source = "preserve authored text" }
			session.__sourceDrafts.SCRIPT = draft
			local fetched = false
			session.__replaceInstances = function()
				fetched = true
				error("Unexpected disk fetch.")
			end
			session.__reconciler.applyPatch = function(_, patch)
				return patch
			end
			const patch = {
				added = {},
				removed = {},
				updated = {
					{ id = "SCRIPT", changedName = "Accepted", changedProperties = {} },
				},
			}
			const applied, message = pcall(ServeSession.__applyPatchBody, session, patch, 11)
			expect(applied).to.equal(false)
			expect(tostring(message):find("without fetching live filesystem replacements", 1, true) ~= nil).to.equal(
				true
			)
			expect(fetched).to.equal(false)
			expect(session.__sourceDrafts.SCRIPT).to.equal(draft)
		end)
		Settings.get = originalGet
		assert(success, problem)
	end)

	it("reports Saved only after an accepted Studio write acknowledgement", function()
		const session, control = fixture("studio-to-fs")
		session:start()
		local answer
		control.writeResult = Promise.new(function(resolve)
			answer = resolve
		end)
		const writing = session:__writePatch(PatchSet.newEmpty())
		expect(session:getSyncResults()[1].status).to.equal("Pending")
		answer({
			messageCursor = 10,
			accepted = { added_instances = {}, removed_instances = {}, updated_instances = {} },
			conflicts = { added_instances = {}, removed_instances = {}, updated_instances = {} },
			errors = {},
		})
		writing:await()
		expect(session:getSyncResults()[1].status).to.equal("Saved")
		expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
		expect(next(session.__writeTickets)).to.equal(nil)
	end)

	for _, outcome in { "collision", "ownership", "transport" } do
		it(`does not restore corrections or report Saved after {outcome}`, function()
			const session, control = fixture("studio-to-fs")
			session:start()
			const draft = { Source = "draft" }
			session.__sourceDrafts.SCRIPT = draft
			const result = {
				messageCursor = 10,
				accepted = { added_instances = {}, removed_instances = {}, updated_instances = {} },
				conflicts = { added_instances = {}, removed_instances = {}, updated_instances = {} },
				errors = {},
			}
			if outcome == "collision" then
				table.insert(
					result.conflicts.updated_instances,
					{ id = "SCRIPT", changed_properties = { Source = { String = "disk correction" } } }
				)
			elseif outcome == "ownership" then
				table.insert(result.errors, { id = "SCRIPT", message = "Not owned." })
			end
			control.writeResult = if outcome == "transport"
				then Promise.reject("Transport failed.")
				else Promise.resolve(result)
			session:__writePatch(PatchSet.newEmpty()):await()
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(session.__sourceDrafts.SCRIPT).to.equal(draft)
			expect(session.__resumeBlocked).to.equal(true)
			expect(session:getSyncResults()[1].status == "Saved").to.equal(false)
			expect(#control.applied).to.equal(1)
		end)
	end
end
