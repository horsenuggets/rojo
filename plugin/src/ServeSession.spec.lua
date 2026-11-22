return function()
	local ServeSession = require(script.Parent.ServeSession)
	local Promise = require(script.Parent.Parent.Packages.Promise)
	local SourceHistory = require(script.Parent.SourceHistory)

	local function sessionState(fields, class)
		if fields.__instanceMap ~= nil and fields.__instanceMap.captureStructuralChanges == nil then
			-- These fixtures replace the instance-observation boundary with plain maps.
			fields.__instanceMap.captureStructuralChanges = function() end
		end
		if fields.__apiContext ~= nil and fields.__apiContext.getMessageCursor == nil then
			fields.__apiContext.getMessageCursor = function()
				return 0
			end
		end
		fields.__sourceHistory = SourceHistory.new({
			Clock = function()
				return 0
			end,
			Horizon = 10,
			MaxBytes = 8192,
			MaxEntries = 32,
		})
		fields.__sourceDrafts = {}
		fields.__sourceRevisions = {}
		fields.__sourcePublicationCursors = setmetatable({}, { __mode = "k" })
		fields.__sourceBaselines = setmetatable({}, { __mode = "k" })
		fields.__sourcePendingCounts = setmetatable({}, { __mode = "k" })
		fields.__remoteSources = {}
		fields.__sourceApplyBatches = setmetatable({}, { __mode = "k" })
		return setmetatable(fields, class)
	end

	local function writeResult(errors, conflicts)
		return {
			messageCursor = 0,
			errors = errors or {},
			accepted = {
				removed_instances = {},
				added_instances = {},
				updated_instances = {},
			},
			conflicts = conflicts or {
				removed_instances = {},
				added_instances = {},
				updated_instances = {},
			},
		}
	end

	local function propertySession(className)
		local control = { cursor = 0, fail = {}, requests = {}, settle = {} }
		local session = ServeSession.new({
			twoWaySync = true,
			apiContext = {
				getMessageCursor = function()
					return control.cursor
				end,
				disconnect = function() end,
				write = function(_self, patch)
					table.insert(control.requests, patch)
					local index = #control.requests
					return Promise.new(function(resolve, reject)
						control.fail[index] = reject
						control.settle[index] = resolve
					end)
				end,
			},
		})
		session.__changeBatcher.__renderSteppedConnection:Disconnect()
		session.__instanceMap.onInstanceChanged = nil
		session.__deferredChanges = nil
		local instance = Instance.new(className)
		session.__instanceMap:insert("EDITED", instance)
		return session, instance, control
	end

	local function structuralSession(propertyName)
		local session, instance, control = propertySession("Actor")
		session.__instanceMap.onChildAdded = nil
		local root = Instance.new("Folder")
		local parents = {}
		for _, id in { "PROBE", "DESTINATION", "DISK" } do
			local parent = Instance.new("Folder")
			parent.Name = id
			parent.Parent = root
			parents[id] = parent
			session.__instanceMap:insert(id, parent)
		end
		session.__instanceMap:insert("ROOT", root)
		instance.Parent = session.__instanceMap.fromIds.PROBE
		session.__instanceMap.knownParents[instance] = instance.Parent
		local field = if propertyName == "Parent" then "changedParent" else "changedName"
		control.edit = function(value)
			instance[propertyName] = if propertyName == "Parent" then session.__instanceMap.fromIds[value] else value
			session:__queueChange(instance, propertyName)
			return session.__localPropertyEdits.EDITED and session.__localPropertyEdits.EDITED[propertyName]
		end
		control.expectValue = function(value)
			expect(instance[propertyName]).to.equal(if propertyName == "Parent" then parents[value] else value)
		end
		control.publish = function(value, cursor)
			return session:__applyPatch({
				added = {},
				removed = {},
				updated = { { id = "EDITED", [field] = value, changedProperties = {} } },
			}, cursor)
		end
		control.answer = function(index, cursor, conflicts)
			local result = writeResult(nil, conflicts)
			result.messageCursor = cursor
			control.settle[index](result)
		end
		control.cleanup = function()
			session:stop()
			instance:Destroy()
			root:Destroy()
		end
		return session, instance, control, field
	end

	local function deferredStructuralSession(propertyName)
		local session, instance, control, field = structuralSession(propertyName)
		local map = session.__instanceMap
		for _, connection in map.instancesToSignal[instance] do
			connection:Disconnect()
		end
		map.observedNames[instance] = instance.Name
		map.observedParents[instance] = instance.Parent
		map.__isGameRunning = function()
			return false
		end
		map.onInstanceChanged = function(changed, property)
			session:__onInstanceChanged(changed, property)
		end
		control.assign = function(value)
			instance[propertyName] = if propertyName == "Parent" then map.fromIds[value] else value
		end
		control.deliver = function()
			map:__maybeFireInstanceChanged(instance, propertyName)
		end
		control.edit = function(value)
			control.assign(value)
			control.deliver()
			return session.__localPropertyEdits.EDITED and session.__localPropertyEdits.EDITED[propertyName]
		end
		return session, instance, control, field
	end

	local function forDeferredStructures(run)
		for _, propertyName in { "Parent", "Name" } do
			local session, instance, control, field = deferredStructuralSession(propertyName)
			local success, problem = pcall(run, propertyName, session, instance, control, field)
			control.cleanup()
			assert(success, `The "{propertyName}" schedule failed. {tostring(problem)}`)
		end
	end

	local function sourceRecoverySession()
		local session, instance, control = propertySession("ModuleScript")
		control.text = "return 'draft'"
		control.checkpoints = {}
		session.__readEditorSource = function()
			return control.text
		end
		session.__apiContext.createSourceCheckpoint = function(_, id, source, expectedSource, cursor)
			table.insert(
				control.checkpoints,
				{ id = id, source = source, expectedSource = expectedSource, cursor = cursor }
			)
			return Promise.new(function(resolve, reject)
				control.finishCheckpoint = resolve
				control.failCheckpoint = reject
			end)
		end
		session.__reconciler.applyPatch = function(_, patch)
			for _, update in patch.updated do
				if update.changedProperties.Source then
					control.text = update.changedProperties.Source.String
				end
			end
			return { added = {}, removed = {}, updated = {} }
		end
		return session, instance, control
	end

	local function sourcePublication(text, operation)
		return {
			added = {},
			removed = {},
			updated = { { id = "EDITED", changedProperties = { Source = { String = text } } } },
			sourceOperations = if operation then { EDITED = operation } else nil,
		}
	end

	-- Holds a first write open so later batches queue behind it, as Studio edits do while
	-- an earlier request is in flight. Statuses are what the app turns into notifications.
	local function queuedPartSession()
		local session, part, control = propertySession("Part")
		local instanceMap = session.__instanceMap
		local root = Instance.new("Folder")
		local other = Instance.new("Part")
		instanceMap.onChildAdded = nil
		part.Parent = root
		other.Parent = root
		instanceMap:insert("ROOT", root)
		instanceMap:insert("OTHER", other)
		instanceMap.knownParents[part] = root
		instanceMap.observedParents[part] = root
		control.other = other
		control.statuses = {}
		control.results = {}
		session:setSyncResultCallback(function(status, result)
			table.insert(control.statuses, status)
			table.insert(control.results, result)
		end)
		control.first = session:__writePatch({
			added = {},
			removed = {},
			updated = { { id = "EDITED", changedProperties = { Transparency = { Float32 = 0.5 } } } },
		})
		control.edit = function(instance, width)
			instance.Size = Vector3.new(width, 4, 2)
			session:__queueChange(instance, "Size")
		end
		control.replace = function()
			session:__applyPatch({
				added = {},
				removed = {},
				updated = { { id = "EDITED", changedClassName = "Folder", changedProperties = {} } },
			}, 1)
			control.folder = instanceMap.fromIds.EDITED
			expect(control.folder.ClassName).to.equal("Folder")
		end
		control.remove = function()
			session:__applyPatch({ added = {}, removed = { "EDITED" }, updated = {} }, 2)
			expect(instanceMap.fromIds.EDITED).to.equal(nil)
		end
		control.sentIds = function(index)
			local ids = {}
			for _, update in control.requests[index].updated do
				table.insert(ids, update.id)
			end
			table.sort(ids)
			return table.concat(ids, ",")
		end
		control.cleanup = function()
			session:stop()
			part:Destroy()
			other:Destroy()
			if control.folder ~= nil then
				control.folder:Destroy()
			end
			root:Destroy()
		end
		return session, part, control
	end

	it("should supersede encoded property batches when disk replaces and removes their instance", function()
		local session, part, control = queuedPartSession()
		local success, problem = pcall(function()
			local queued = {}
			for width = 8, 13 do
				control.edit(part, width)
				table.insert(queued, session:flushChanges())
			end
			expect(#control.requests).to.equal(1)
			control.replace()
			control.settle[1](writeResult())
			assert(control.first:await())
			control.remove()
			-- Only the first write was sent. The six Size batches lost their Part before
			-- dispatch, so none of them can come back as a conflict or an unknown instance.
			expect(#control.requests).to.equal(1)
			assert(Promise.all(queued):await())
			expect(table.concat(control.statuses, ",")).to.equal(
				"Saved,Superseded,Superseded,Superseded,Superseded,Superseded,Superseded"
			)
			for index = 2, #control.results do
				local result = control.results[index]
				expect(#result.errors).to.equal(0)
				expect(result.acknowledgement).to.equal(nil)
				expect(result.superseded.updated[1].id).to.equal("EDITED")
			end

			-- Superseded tickets still complete, so a later edit is not stuck behind them.
			control.edit(control.other, 3)
			local later = session:flushChanges()
			expect(#control.requests).to.equal(2)
			expect(control.sentIds(2)).to.equal("OTHER")
			control.settle[2](writeResult())
			assert(later:await())
			expect(control.statuses[#control.statuses]).to.equal("Saved")
		end)
		control.cleanup()
		assert(success, problem)
	end)

	it("should send only the operations whose encoded targets survive until dispatch", function()
		-- Every combination of class replacement, removal, and a surviving neighbor in the
		-- same batch. Only the replaced or removed target's intent may be dropped.
		for row = 0, 7 do
			local replaced, removed, survivor = row % 2 == 1, row // 2 % 2 == 1, row // 4 == 1
			local session, part, control = queuedPartSession()
			local success, problem = pcall(function()
				control.edit(part, 8)
				if survivor then
					control.edit(control.other, 3)
				end
				local queued = session:flushChanges()
				expect(#control.requests).to.equal(1)
				if replaced then
					control.replace()
				end
				if removed then
					control.remove()
				end
				control.settle[1](writeResult())
				assert(control.first:await())

				local stale = replaced or removed
				local expectedIds = {}
				if not stale then
					table.insert(expectedIds, "EDITED")
				end
				if survivor then
					table.insert(expectedIds, "OTHER")
				end
				-- A batch whose every operation was superseded sends no empty request.
				expect(#control.requests).to.equal(if #expectedIds > 0 then 2 else 1)
				if #expectedIds > 0 then
					expect(control.sentIds(2)).to.equal(table.concat(expectedIds, ","))
					control.settle[2](writeResult())
				end
				assert(queued:await())
				expect(table.concat(control.statuses, ",")).to.equal(
					if not stale then "Saved,Saved" elseif survivor then "Saved,Superseded,Saved" else "Saved,Superseded"
				)
				if stale then
					local superseded = control.results[2].superseded
					expect(#superseded.updated).to.equal(1)
					expect(superseded.updated[1].id).to.equal("EDITED")
				end
			end)
			control.cleanup()
			assert(
				success,
				`Replaced {replaced}, removed {removed}, and survivor {survivor} failed. {tostring(problem)}`
			)
		end
	end)

	it("should still report a genuine outcome for a surviving instance beside a superseded one", function()
		for _, staleNeighbor in { false, true } do
			for _, outcome in { "Conflicted", "Failed" } do
				local session, part, control = queuedPartSession()
				local success, problem = pcall(function()
					control.edit(part, 8)
					control.edit(control.other, 3)
					local queued = session:flushChanges()
					if staleNeighbor then
						control.remove()
					end
					control.settle[1](writeResult())
					assert(control.first:await())
					expect(control.sentIds(2)).to.equal(if staleNeighbor then "OTHER" else "EDITED,OTHER")
					control.settle[2](if outcome == "Conflicted"
						then writeResult(nil, {
							removed_instances = {},
							added_instances = {},
							updated_instances = { { id = "OTHER", changed_properties = {} } },
						})
						else writeResult({ { id = "OTHER", property = "Size", message = "Rejected" } }))
					assert(queued:await())
					expect(table.concat(control.statuses, ",")).to.equal(
						if staleNeighbor then `Saved,Superseded,{outcome}` else `Saved,{outcome}`
					)
				end)
				control.cleanup()
				assert(success, `Stale neighbor {staleNeighbor} with "{outcome}" failed. {tostring(problem)}`)
			end
		end
	end)

	it("should supersede a queued deletion only when the filesystem removed it first", function()
		for _, removedFirst in { false, true } do
			local session, part, control = queuedPartSession()
			local success, problem = pcall(function()
				part.Parent = nil
				session:__queueChange(part, "Parent")
				local queued = session:flushChanges()
				expect(session.__pendingDeletions.EDITED).to.equal(part)
				if removedFirst then
					control.remove()
				end
				control.settle[1](writeResult())
				assert(control.first:await())
				expect(#control.requests).to.equal(if removedFirst then 1 else 2)
				if not removedFirst then
					expect(control.requests[2].removed[1]).to.equal("EDITED")
					control.settle[2](writeResult())
				end
				assert(queued:await())
				expect(table.concat(control.statuses, ",")).to.equal(
					if removedFirst then "Saved,Superseded" else "Saved,Saved"
				)
				expect(session.__pendingDeletions.EDITED).to.equal(nil)
			end)
			control.cleanup()
			assert(success, `Removed first {removedFirst} failed. {tostring(problem)}`)
		end
	end)

	it("should supersede a batch whose target a publication changes while omitted properties replay", function()
		for _, removal in { false, true } do
			local session, part, control = queuedPartSession()
			local success, problem = pcall(function()
				-- Replaying omitted properties runs precommit hooks, which may yield. A
				-- publication landing meanwhile changes the target after it was encoded.
				local finishOmitted = session.__finishOmittedLocalProperties
				session.__finishOmittedLocalProperties = function(...)
					if removal then
						control.remove()
					else
						control.replace()
					end
					return finishOmitted(...)
				end
				control.edit(part, 8)
				local queued = session:flushChanges()
				control.settle[1](writeResult())
				assert(control.first:await())
				expect(#control.requests).to.equal(1)
				assert(queued:await())
				expect(table.concat(control.statuses, ",")).to.equal("Saved,Superseded")
			end)
			control.cleanup()
			assert(success, `Removal {removal} failed. {tostring(problem)}`)
		end
	end)

	it("should keep a newer instance's deletion guard when an older deletion under its id settles", function()
		for _, outcome in { "Superseded", "Mixed", "Saved", "Failed" } do
			local session, part, control = queuedPartSession()
			local success, problem = pcall(function()
				local root = part.Parent
				local dispatched = outcome == "Saved" or outcome == "Failed"
				part.Parent = nil
				session:__queueChange(part, "Parent")
				if outcome == "Mixed" then
					control.edit(control.other, 3)
				end
				local older = session:flushChanges()
				if dispatched then
					control.settle[1](writeResult())
					assert(control.first:await())
					expect(control.requests[2].removed[1]).to.equal("EDITED")
				end

				-- Studio undoes the Part's deletion, the filesystem replaces it, and Studio
				-- then deletes the replacement Folder under the same id.
				part.Parent = root
				control.replace()
				local folder = control.folder
				expect(folder.Parent).to.equal(root)
				folder.Parent = nil
				session:__queueChange(folder, "Parent")
				local newer = session:flushChanges()
				expect(session.__pendingDeletions.EDITED).to.equal(folder)

				if outcome == "Failed" then
					control.fail[2]("The connection dropped")
				elseif dispatched then
					control.settle[2](writeResult())
				else
					control.settle[1](writeResult())
					assert(control.first:await())
					if outcome == "Mixed" then
						expect(control.sentIds(2)).to.equal("OTHER")
						control.settle[2](writeResult())
					end
				end
				older:await()
				expect(session.__pendingDeletions.EDITED).to.equal(folder)

				-- An undo before the Folder's deletion announcement keeps it restored.
				local index = #control.requests
				expect(control.requests[index].removed[1]).to.equal("EDITED")
				folder.Parent = root
				session:__applyPatch({ added = {}, removed = { "EDITED" }, updated = {} }, 3)
				expect(folder.Parent).to.equal(root)
				control.settle[index](writeResult())
				assert(newer:await())
			end)
			control.cleanup()
			assert(success, `Outcome "{outcome}" failed. {tostring(problem)}`)
		end
	end)

	it("should neither send a stopped session's batch nor supersede a new session's write", function()
		local old, part, oldControl = queuedPartSession()
		local new, unused, newControl = propertySession("Part")
		local success, problem = pcall(function()
			oldControl.edit(part, 8)
			local stale = old:flushChanges()
			old:stop()
			oldControl.settle[1](writeResult())
			stale:await()
			expect(#oldControl.requests).to.equal(1)

			-- The reconnected session tracks the same instance under the same id.
			local statuses = {}
			new:setSyncResultCallback(function(status)
				table.insert(statuses, status)
			end)
			new.__instanceMap:insert("EDITED", part)
			part.Size = Vector3.new(9, 4, 2)
			new:__queueChange(part, "Size")
			local write = new:flushChanges()
			expect(#newControl.requests).to.equal(1)
			expect(newControl.requests[1].updated[1].id).to.equal("EDITED")
			newControl.settle[1](writeResult())
			assert(write:await())
			expect(table.concat(statuses, ",")).to.equal("Saved")
			expect(#oldControl.requests).to.equal(1)
		end)
		oldControl.cleanup()
		new:stop()
		unused:Destroy()
		assert(success, problem)
	end)

	it("should keep a secured recovery checkpoint while superseding the script's queued Source write", function()
		local session, module, control = sourceRecoverySession()
		local instanceMap = session.__instanceMap
		local root = Instance.new("Folder")
		instanceMap.onChildAdded = nil
		module.Parent = root
		instanceMap:insert("ROOT", root)
		instanceMap.knownParents[module] = root
		instanceMap.observedParents[module] = root
		session.__reconciler.applyPatch = function(_, patch)
			for _, id in patch.removed do
				instanceMap:destroyId(id)
			end
			return { added = {}, removed = {}, updated = {} }
		end
		local statuses = {}
		session:setSyncResultCallback(function(status)
			table.insert(statuses, status)
		end)
		local begun = 0
		local begin = session.__sourceHistory.begin
		session.__sourceHistory.begin = function(history, ...)
			begun += 1
			return begin(history, ...)
		end
		local success, problem = pcall(function()
			local first = session:__writePatch({ added = {}, removed = {}, updated = {} })
			session:__queueChange(module, "Source")
			local queued = session:flushChanges()
			expect(#control.requests).to.equal(1)

			local applying = session:__applyPatch({ added = {}, removed = { "EDITED" }, updated = {} }, 1)
			expect(#control.checkpoints).to.equal(1)
			expect(control.checkpoints[1].source).to.equal("return 'draft'")
			control.finishCheckpoint({ checkpointId = "secured" })
			assert(applying:await())
			expect(instanceMap.fromIds.EDITED).to.equal(nil)

			control.settle[1](writeResult())
			assert(first:await())
			-- The removed script's draft lives in its checkpoint. Its write is neither sent,
			-- begun as Source history, nor reported as saved.
			expect(#control.requests).to.equal(1)
			assert(queued:await())
			expect(begun).to.equal(0)
			expect(table.concat(statuses, ",")).to.equal("Conflicted,Saved,Superseded")
			expect(#control.checkpoints).to.equal(1)
		end)
		session:stop()
		module:Destroy()
		root:Destroy()
		assert(success, problem)
	end)

	it("should partition superseded operations without changing intact patches", function()
		local kept = Instance.new("Folder")
		local replaced = Instance.new("Folder")
		local replacement = Instance.new("Folder")
		local patch = {
			added = { NEW = { parent = "KEPT" } },
			removed = { "REMOVED", "KEPT" },
			updated = {
				{ id = "KEPT", changedProperties = {} },
				{ id = "REPLACED", changedProperties = {} },
				{ id = "UNTRACKED", changedProperties = {} },
			},
		}
		local before = { KEPT = kept, REMOVED = replaced, REPLACED = replaced }
		local targets = ServeSession.captureOperationTargets(patch, before)
		local intact, none = ServeSession.partitionSupersededOperations(patch, targets, before)
		expect(intact).to.equal(patch)
		expect(#none.updated + #none.removed).to.equal(0)

		local after = { KEPT = kept, REPLACED = replacement, UNTRACKED = kept }
		for _ = 1, 2 do
			local sent, superseded = ServeSession.partitionSupersededOperations(patch, targets, after)
			expect(sent.added).to.equal(patch.added)
			expect(table.concat(sent.removed, ",")).to.equal("KEPT")
			expect(#sent.updated).to.equal(2)
			expect(sent.updated[1].id).to.equal("KEPT")
			expect(sent.updated[2].id).to.equal("UNTRACKED")
			expect(table.concat(superseded.removed, ",")).to.equal("REMOVED")
			expect(#superseded.updated).to.equal(1)
			expect(superseded.updated[1].id).to.equal("REPLACED")
			expect(#patch.updated).to.equal(3)
		end
		kept:Destroy()
		replaced:Destroy()
		replacement:Destroy()
	end)

	it("should capture a preparing batch ticket before serialization yields and drain omitted tickets", function()
		for _, omitted in { false, true } do
			local session, instance, control = sourceRecoverySession()
			local calls = {}
			session.__apiContext.write = function(_, _, cursor, _, _, predecessor)
				table.insert(calls, { Cursor = cursor, Predecessor = predecessor })
				local result = writeResult()
				result.acknowledgement = if #calls == 1 then "first-proof" else "second-proof"
				return Promise.resolve(result)
			end
			local success, problem = pcall(function()
				session:__queueChange(instance, "Name")
				local batcher = session.__changeBatcher
				local context = batcher.__onBatchTaken({ [instance] = { Name = true } })
				batcher.__pendingPropertyChanges = {}
				-- The next document edit occurs while the first batch is being encoded.
				control.text = "next draft"
				session:__queueChange(instance, "Source")
				local captured = session:__takeLocalPropertyEdits({ [instance] = { Source = true } })
				local writing = session:__writePatch(sourcePublication(control.text), 0, captured)
				expect(#calls).to.equal(0)
				local first = { added = {}, removed = {}, updated = {} }
				if not omitted then
					first.updated = { { id = "EDITED", changedName = "Renamed", changedProperties = {} } }
				end
				batcher.__onBatchPrepared(first, context)
				if not omitted then
					assert(batcher.__onChangesFlushed(first, context):await())
				end
				assert(writing:await())
				expect(#calls).to.equal(if omitted then 1 else 2)
				expect(calls[#calls].Predecessor).to.equal(if omitted then nil else "first-proof")
				expect(context.ticket.Parent).to.equal(nil)
				expect(context.ticket.Resolve).to.equal(nil)
			end)
			session:stop()
			instance:Destroy()
			assert(success, problem)
		end
	end)

	it("should never substitute a later acknowledgement for captured direct write dependencies", function()
		local session, instance, control = sourceRecoverySession()
		local calls, settles = {}, {}
		session.__apiContext.write = function(_, _, cursor, _, _, predecessor, baselines)
			table.insert(calls, { Cursor = cursor, Predecessor = predecessor, Baselines = baselines })
			return Promise.new(function(resolve)
				table.insert(settles, resolve)
			end)
		end
		local success, problem = pcall(function()
			local first = session:__writePatch(sourcePublication("first"), 0)
			local second = session:__writePatch(sourcePublication("second"), 0)
			-- This captures the second ticket, not the latest capability at dispatch.
			control.text = "captured"
			session:__rememberLocalProperty(instance, "Source")
			local captured = session:__takeLocalPropertyEdits({ [instance] = { Source = true } })
			local unrelated = session:__writePatch(sourcePublication("unrelated"), 0)
			local last = session:__writePatch(sourcePublication(control.text), 0, captured)
			for index, proof in { "one", "two", "unrelated", "last" } do
				local result = writeResult()
				result.acknowledgement = proof
				settles[index](result)
			end
			assert(
				first:await() and second:await() and unrelated:await() and last:await(),
				"Captured writes must finish."
			)
			expect(calls[2].Predecessor).to.equal("one")
			expect(calls[3].Predecessor).to.equal("two")
			expect(calls[4].Predecessor).to.equal("two")
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should await a candidate baseline for typing before the remote terminal outcome", function()
		for _, kind in { "committed", "failed", "locally-cancelled", "retried", "session-stopped" } do
			local committed = kind == "committed"
			local session, instance, control = sourceRecoverySession()
			local _, generation = session.__instanceMap:getSourceRevision(instance)
			local operation = { Status = "pending", Expected = control.text, Value = "remote" }
			local calls = {}
			session.__apiContext.write = function(_, _, _, _, _, _, baselines)
				table.insert(calls, baselines.EDITED)
				return Promise.resolve(writeResult())
			end
			local success, problem = pcall(function()
				session.__sourceBaselines[instance] = { Cursor = 3, Revision = 0 }
				local operations = { Cursors = { EDITED = 7 } }
				session.__sourceApplyBatches[coroutine.running()] = operations
				session:__onSourceScheduled(instance, operation)
				session.__sourceApplyBatches[coroutine.running()] = nil
				control.text = "remote"
				session:__onInstanceChanged(instance, "Source", {
					Operation = operation,
					Generation = generation,
					Revision = 1,
					Value = control.text,
				})
				control.text = "new local typing"
				session:__onInstanceChanged(instance, "Source", {
					Generation = generation,
					Revision = 2,
					Value = control.text,
				})
				local captured = session:__takeLocalPropertyEdits({ [instance] = { Source = true } })
				local writing = session:__writePatch(sourcePublication(control.text), 0, captured)
				expect(#calls).to.equal(0)
				if kind == "session-stopped" then
					session:stop()
				end
				operation.Status = if kind == "retried"
					then "committed"
					elseif kind == "session-stopped" then "locally-cancelled"
					else kind
				operation.CommittedRevision = if committed then 1 elseif kind == "retried" then 3 else nil
				session:__onSourceOutcome(instance, operation)
				assert(writing:await())
				if kind == "session-stopped" then
					expect(#calls).to.equal(0)
				else
					expect(calls[1]).to.equal(if committed then 7 else 3)
				end
				expect(control.text).to.equal("new local typing")
			end)
			session:stop()
			instance:Destroy()
			assert(success, problem)
		end
	end)

	it("should bound completed lineage without retaining ticket parents or resolvers", function()
		local session, instance = sourceRecoverySession()
		local success, problem = pcall(function()
			for index = 1, 600 do
				local ticket = session:__newWriteTicket({ session.__writeHead or false })
				session:__resolveWriteTicket(ticket, `proof-{index}`)
				session:__completeWriteTicket(ticket)
				local resolved, lineage = ticket.Promise:await()
				assert(resolved, "The ticket must resolve.")
				expect(#lineage).to.equal(math.min(index, 256))
				expect(ticket.Parent).to.equal(nil)
				expect(ticket.Resolve).to.equal(nil)
				expect(ticket.PreviousWrite).to.equal(nil)
				expect(ticket.Complete).to.equal(nil)
				expect(next(session.__writeTickets)).to.equal(nil)
			end
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should retain only complete common lineage across mixed batch intents and partial failures", function()
		for _, failed in { false, true } do
			local session, instance = sourceRecoverySession()
			local calls = {}
			session.__apiContext.write = function(_, _, _, _, _, predecessor)
				table.insert(calls, predecessor or false)
				local result = writeResult()
				result.acknowledgement = `proof-{#calls}`
				if failed and #calls == 2 then
					result.errors = { { id = "EDITED", property = "Source", message = "Rejected." } }
					result.acknowledgement = nil
				end
				return Promise.resolve(result)
			end
			local success, problem = pcall(function()
				assert(session:__writePatch(sourcePublication("first"), 0):await())
				-- Capture one intent before another successful (or partial) write.
				session:__rememberLocalProperty(instance, "Source")
				local source = session:__takeLocalPropertyEdits({ [instance] = { Source = true } })
				assert(session:__writePatch(sourcePublication("second"), 0):await())
				session:__rememberLocalProperty(instance, "Archivable")
				local other = session:__takeLocalPropertyEdits({ [instance] = { Archivable = true } })
				source.EDITED.Archivable = other.EDITED.Archivable
				assert(session:__writePatch(sourcePublication("mixed"), 0, source):await())
				expect(calls[3]).to.equal("proof-1")
			end)
			session:stop()
			instance:Destroy()
			assert(success, problem)
		end
	end)

	it("should advance metadata-only context only for unchanged consumed documents", function()
		for _, edited in { false, true } do
			local session, instance, control = sourceRecoverySession()
			local success, problem = pcall(function()
				session.__sourceBaselines[instance] = { Cursor = 2, Revision = 0 }
				local captured
				if edited then
					local original = control.text
					control.text = "edit"
					session:__rememberLocalProperty(instance, "Source")
					control.text = original
					session:__rememberLocalProperty(instance, "Source")
					captured = session:__takeLocalPropertyEdits({ [instance] = { Source = true } })
				end
				local patch = {
					added = {},
					removed = {},
					updated = {
						{ id = "EDITED", changedProperties = {}, changedMetadata = { ignoreUnknownInstances = true } },
					},
				}
				session:__applyPatchNow(patch, 8)
				expect(session.__sourceBaselines[instance].Cursor).to.equal(if edited then 2 else 8)
				if captured then
					expect(captured.EDITED.Source.sourceBaseline.Cursor).to.equal(2)
				end
			end)
			session:stop()
			instance:Destroy()
			assert(success, problem)
		end
	end)

	it("should not consume a metadata context attached to suppressed Source or a pending proposal", function()
		for _, kind in { "echo", "pending", "failed" } do
			local session, instance, control = sourceRecoverySession()
			local success, problem = pcall(function()
				session.__sourceBaselines[instance] = { Cursor = 2, Revision = 0 }
				local patch = {
					added = {},
					removed = {},
					updated = {
						{ id = "EDITED", changedProperties = {}, changedMetadata = { ignoreUnknownInstances = true } },
					},
				}
				local operation
				if kind == "echo" then
					session.__sourceHistory:begin(
						"old",
						{ EDITED = { Instance = instance, Revision = 0, Source = "old" } }
					)
					session.__sourceRevisions.EDITED = { Instance = instance, Revision = 1 }
					session.__sourceBaselines[instance].Revision = 1
					patch.updated[1].changedProperties.Source = { String = "old" }
					patch.sourceOperations = { EDITED = "old" }
				elseif kind == "pending" then
					operation = { Status = "pending", Value = "not yet consumed" }
					session:__onSourceScheduled(instance, operation)
				else
					session.__reconciler.applyPatch = function()
						return patch
					end
					session.__replaceInstances = function()
						return false
					end
				end
				session:__applyPatchNow(patch, 8)
				expect(session.__sourceBaselines[instance].Cursor).to.equal(2)
				expect(control.text).to.equal("return 'draft'")
				if operation then
					operation.Status = "superseded"
					session:__onSourceOutcome(instance, operation)
				end
			end)
			session:stop()
			instance:Destroy()
			assert(success, problem)
		end
	end)

	it("should not turn a partially applied packet or an echo into consumed Source", function()
		local session, instance, control = sourceRecoverySession()
		local other = Instance.new("ModuleScript")
		session.__instanceMap:insert("OTHER", other)
		local success, problem = pcall(function()
			session.__sourceBaselines[instance] = { Cursor = 1, Revision = 0 }
			session.__sourceBaselines[other] = { Cursor = 1, Revision = 0 }
			local operations = { Cursors = { EDITED = 4, OTHER = 5 } }
			session.__sourceApplyBatches[coroutine.running()] = operations
			local first = { Status = "pending", Value = control.text, NoChange = true }
			local second = { Status = "pending", Value = "unapplied" }
			session:__onSourceScheduled(instance, first)
			session:__onSourceScheduled(other, second)
			session.__sourceApplyBatches[coroutine.running()] = nil
			first.Status = "committed"
			session:__onSourceOutcome(instance, first)
			control.cursor = 0
			expect(session.__sourceBaselines[instance].Cursor).to.equal(4)
			expect(session.__sourceBaselines[other].Cursor).to.equal(1)
			second.Status = "failed"
			session:__onSourceOutcome(other, second)
			expect(session.__sourceBaselines[other].Cursor).to.equal(1)
			session:__rememberLocalProperty(instance, "Source")
			session.__sourceHistory:begin(
				"older",
				{ EDITED = { Instance = instance, Revision = 0, Source = control.text } }
			)
			session:__applyPatch(sourcePublication("older bytes", "older"), 8)
			expect(session.__sourcePublicationCursors[instance]).to.equal(8)
			expect(session.__sourceBaselines[instance].Cursor).to.equal(4)
		end)
		session:stop()
		instance:Destroy()
		other:Destroy()
		assert(success, problem)
	end)

	it("should resolve preparing tickets on session cancellation without sending dependent drafts", function()
		local session, instance, control = sourceRecoverySession()
		local success, problem = pcall(function()
			session:__queueChange(instance, "Name")
			local context = session.__changeBatcher.__onBatchTaken({ [instance] = { Name = true } })
			session:__rememberLocalProperty(instance, "Source")
			local captured = session:__takeLocalPropertyEdits({ [instance] = { Source = true } })
			local writing = session:__writePatch(sourcePublication(control.text), 0, captured)
			expect(#control.requests).to.equal(0)
			session:stop()
			assert(writing:await())
			expect(#control.requests).to.equal(0)
			expect(context.ticket.Resolve).to.equal(nil)
			expect(session:getSyncResults()[1].status).to.equal("Failed")
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it(
		"should establish initial READ baselines only on matching documents without rewriting captured intent",
		function()
			for _, matches in { false, true } do
				local session, instance, control = sourceRecoverySession()
				session.__apiContext.read = function()
					return Promise.resolve({
						messageCursor = 7,
						instances = {
							EDITED = {
								Properties = {
									Source = { String = if matches then control.text else "unapplied server text" },
								},
							},
						},
					})
				end
				session.__apiContext.setMessageCursor = function(_, cursor)
					control.cursor = cursor
				end
				session.__reconciler.hydrate = function() end
				session.__reconciler.diff = function()
					return true, { added = {}, removed = {}, updated = {} }
				end
				local success, problem = pcall(function()
					session:__rememberLocalProperty(instance, "Source")
					local captured = session:__takeLocalPropertyEdits({ [instance] = { Source = true } })
					assert(session:__initialSync({ rootInstanceId = "ROOT" }):await())
					local baseline = session.__sourceBaselines[instance]
					expect(baseline and baseline.Cursor).to.equal(if matches then 7 else nil)
					expect(captured.EDITED.Source.sourceBaseline).to.equal(nil)
					session.__apiContext.write = function(_, _, _, _, _, _, baselines)
						expect(next(baselines)).to.equal(nil)
						return Promise.resolve(writeResult())
					end
					assert(session:__writePatch(sourcePublication(control.text), 0, captured):await())
				end)
				session:stop()
				instance:Destroy()
				assert(success, problem)
			end
		end
	)

	it("should baseline only successfully reified new matching documents", function()
		for _, kind in { "new", "existing", "unapplied", "pending", "committed" } do
			local session, instance, control = sourceRecoverySession()
			local target = Instance.new("ModuleScript")
			local operation
			local patch = {
				added = {
					EDITED = {
						Id = "EDITED",
						Name = "Module",
						ClassName = "ModuleScript",
						Children = {},
						Properties = { Source = { String = control.text } },
					},
				},
				removed = {},
				updated = {},
			}
			session.__reconciler.applyPatch = function()
				if kind ~= "existing" then
					session.__instanceMap:insert("EDITED", target)
				end
				if kind == "pending" or kind == "committed" then
					operation = { Status = "pending", Value = control.text, Expected = "before reification" }
					session:__onSourceScheduled(target, operation)
				end
				return if kind == "unapplied" then patch else { added = {}, removed = {}, updated = {} }
			end
			session.__replaceInstances = function()
				return false
			end
			local success, problem = pcall(function()
				local applying = session:__applyPatchNow(patch, 9)
				local baseline = session.__sourceBaselines[target]
				expect(baseline and baseline.Cursor).to.equal(if kind == "new" then 9 else nil)
				expect(session.__sourceBaselines[instance]).to.equal(nil)
				if operation then
					operation.Status = if kind == "committed" then "committed" else "locally-cancelled"
					session:__onSourceOutcome(target, operation)
					expect(applying:await()).to.equal(kind == "committed")
					local consumed = session.__sourceBaselines[target]
					expect(consumed and consumed.Cursor).to.equal(if kind == "committed" then 9 else nil)
				end
			end)
			session:stop()
			instance:Destroy()
			target:Destroy()
			assert(success, problem)
		end
	end)

	it("should revalidate document intent after a precommit hook edits the buffer", function()
		local session, instance, control = sourceRecoverySession()
		session.__applyPatchNow = ServeSession.__applyPatchNow
		session.__reconciler.applyPatch = function(_, patch)
			for _, update in patch.updated do
				if update.changedProperties.Source then
					control.text = update.changedProperties.Source.String
				end
			end
			return { added = {}, removed = {}, updated = {} }
		end
		session:hookPrecommit(function()
			control.text = "new typing during hook"
			session:__queueChange(instance, "Source")
		end)
		local success, problem = pcall(function()
			session:__applyPatchNow(sourcePublication("filesystem"), 1)
			expect(control.text).to.equal("new typing during hook")
			expect(session.__localPropertyEdits.EDITED.Source.serverValue.String).to.equal("filesystem")
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should defer a candidate remote notification until its actual commit is confirmed", function()
		local session, instance, control = sourceRecoverySession()
		local _, generation = session.__instanceMap:getSourceRevision(instance)
		local operation =
			{ Status = "pending", Expected = control.text, Value = "return 'filesystem'", Generation = generation }
		local operations = {}
		session.__sourceApplyBatches[coroutine.running()] = operations
		session:__onSourceScheduled(instance, operation)
		session.__sourceApplyBatches[coroutine.running()] = nil
		local success, problem = pcall(function()
			control.text = operation.Value
			session:__onInstanceChanged(
				instance,
				"Source",
				{ Operation = operation, Generation = generation, Revision = 1, Value = operation.Value }
			)
			expect(session.__changeBatcher:isEmpty()).to.equal(true)
			expect(session.__sourceDrafts.EDITED ~= nil).to.equal(true)
			operation.Status = "committed"
			operation.CommittedRevision = 1
			session:__onSourceOutcome(instance, operation)
			expect(session:__awaitSourceOperations(operations):await()).to.equal(true)
			expect(session.__sourceDrafts.EDITED).to.equal(nil)
			expect(session.__localPropertyEdits.EDITED).to.equal(nil)
			expect(session.__changeBatcher:isEmpty()).to.equal(true)
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should promote a rejected candidate observation to genuine local intent", function()
		local session, instance, control = sourceRecoverySession()
		local _, generation = session.__instanceMap:getSourceRevision(instance)
		local operation = { Status = "pending", Expected = "older", Value = control.text, Generation = generation }
		local success, problem = pcall(function()
			session:__onSourceScheduled(instance, operation)
			session:__onInstanceChanged(
				instance,
				"Source",
				{ Operation = operation, Generation = generation, Revision = 1, Value = control.text }
			)
			expect(session.__changeBatcher:isEmpty()).to.equal(true)
			operation.Status = "locally-cancelled"
			operation.Reason = "local-edit"
			session:__onSourceOutcome(instance, operation)
			expect(session.__changeBatcher:isEmpty()).to.equal(false)
			expect(session.__sourceDrafts.EDITED.Source).to.equal(control.text)
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should retire a secured draft after authoritative Source commits without a notification", function()
		local session, instance, control = sourceRecoverySession()
		local success, problem = pcall(function()
			session:__rememberLocalProperty(instance, "Source")
			session.__sourceDrafts.EDITED.CheckpointSource = control.text
			session.__localPropertyEdits = {}
			local operation = { Status = "pending", Expected = control.text, Value = "return 'disk'" }
			session:__onSourceScheduled(instance, operation)
			control.text = operation.Value
			operation.Status = "committed"
			session:__onSourceOutcome(instance, operation)
			expect(session.__sourceDrafts.EDITED).to.equal(nil)
			expect(session:__checkpointDisplacedSources(sourcePublication("return 'next disk revision'"), 2)).to.equal(
				nil
			)
			expect(#control.checkpoints).to.equal(0)
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should acknowledge no-op Source writes without clearing deferred or conflicted drafts", function()
		for _, deferred in { false, true } do
			for _, conflict in { false, true } do
				local result = writeResult(
					if deferred
						then { { id = "SCRIPT", property = "Source", kind = "sourceSerializationDeferred" } }
						else nil
				)
				if conflict then
					result.conflicts.updated_instances =
						{ { id = "ANCESTOR", changedParent = "OTHER", changed_properties = {} } }
				end
				expect(ServeSession.sourceWriteSucceeded(result, "SCRIPT", { "ANCESTOR" })).to.equal(
					not deferred and not conflict
				)
			end
		end
	end)

	it("should suppress a proven old Source echo even after newer intent was acknowledged", function()
		local session, instance, control = sourceRecoverySession()
		local success, problem = pcall(function()
			session:__rememberLocalProperty(instance, "Source")
			session.__sourceHistory:begin(
				"old",
				{ EDITED = { Instance = instance, Revision = 1, Source = control.text } }
			)
			session.__sourceHistory:finish("old")
			control.text = "return 'newer saved text'"
			session:__rememberLocalProperty(instance, "Source")
			session.__localPropertyEdits = {}
			session.__sourceDrafts = {}
			session:__applyPatch(sourcePublication("return 'draft'", "old"), 1)
			expect(control.text).to.equal("return 'newer saved text'")
			expect(#control.checkpoints).to.equal(0)
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should keep ambiguous transport outcomes pinned until their session ends", function()
		local session, instance, control = sourceRecoverySession()
		local clock = 0
		local operation
		session.__sourceHistory = SourceHistory.new({
			Clock = function()
				return clock
			end,
			Horizon = 10,
			MaxBytes = 8192,
			MaxEntries = 32,
		})
		session.__apiContext.write = function(_self, _patch, _cursor, _authority, identity)
			operation = identity
			return Promise.reject("The acknowledgement was lost.")
		end
		local success, problem = pcall(function()
			session:__rememberLocalProperty(instance, "Source")
			session:__writePatch(sourcePublication(control.text), 0):await()
			clock = 100
			control.text = "return 'newer draft'"
			session:__rememberLocalProperty(instance, "Source")
			expect(session.__sourceHistory:classify("EDITED", operation, instance, 2)).to.equal("Echo")
			session:__applyPatch(sourcePublication("return 'draft'", operation), 1)
			expect(control.text).to.equal("return 'newer draft'")
			expect(#control.checkpoints).to.equal(0)
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should retain pending Source intent across a structural refresh that preserves its identity", function()
		local session, instance, control = sourceRecoverySession()
		local parent = Instance.new("Folder")
		instance.Parent = parent
		session.__instanceMap:insert("PARENT", parent)
		local success, problem = pcall(function()
			session:__rememberLocalProperty(instance, "Source")
			local entry = session.__localPropertyEdits.EDITED.Source
			local patch = sourcePublication("return 'refreshed filesystem'")
			table.insert(patch.updated, { id = "PARENT", changedName = "MovedParent", changedProperties = {} })
			local filtered = session:__protectLocalProperties(patch, 1)
			expect(session.__localPropertyEdits.EDITED.Source).to.equal(entry)
			expect(#filtered.updated).to.equal(1)
			expect(filtered.updated[1].changedName).to.equal("MovedParent")
			expect(session:__checkpointDisplacedSources(filtered, 1)).to.equal(nil)
			expect(control.text).to.equal("return 'draft'")
		end)
		session:stop()
		parent:Destroy()
		assert(success, problem)
	end)

	it("should discover initial open drafts before structural replacement or subtree removal", function()
		for _, replacingClass in { false, true } do
			local session, instance, control = sourceRecoverySession()
			local parent = Instance.new("Folder")
			instance.Parent = parent
			session.__initialSourceRecovery = true
			session.__hasOpenScriptDocument = function(_self, target)
				return target == instance
			end
			local success, problem = pcall(function()
				local patch = if replacingClass
					then {
						added = {},
						removed = {},
						updated = { { id = "EDITED", changedClassName = "Folder", changedProperties = {} } },
					}
					else { added = {}, removed = { parent }, updated = {} }
				local securing = session:__checkpointDisplacedSources(patch, 1)
				expect(#control.checkpoints).to.equal(1)
				expect(control.checkpoints[1].source).to.equal("return 'draft'")
				control.finishCheckpoint({ checkpointId = "initial-draft" })
				expect(securing:await()).to.equal(true)
			end)
			session:stop()
			parent:Destroy()
			assert(success, problem)
		end
	end)

	it("should refuse initial removal when an open draft has no checkpointable identity", function()
		local session, instance, control = sourceRecoverySession()
		local unknown = Instance.new("ModuleScript")
		session.__initialSourceRecovery = true
		session.__hasOpenScriptDocument = function(_self, target)
			return target == unknown
		end
		local success, problem = pcall(function()
			local securing =
				session:__checkpointDisplacedSources({ added = {}, removed = { unknown }, updated = {} }, 1)
			expect(securing:await()).to.equal(false)
			expect(#control.checkpoints).to.equal(0)
			expect(control.text).to.equal("return 'draft'")
		end)
		session:stop()
		instance:Destroy()
		unknown:Destroy()
		assert(success, problem)
	end)

	it("should secure the exact editor draft before applying a genuine filesystem revision", function()
		local session, instance, control = sourceRecoverySession()
		local success, problem = pcall(function()
			session:__rememberLocalProperty(instance, "Source")
			session.__localPropertyEdits = {}
			local applying = session:__applyPatch(sourcePublication("return 'filesystem'"), 1)
			expect(control.text).to.equal("return 'draft'")
			expect(control.checkpoints[1].source).to.equal("return 'draft'")
			expect(control.checkpoints[1].expectedSource).to.equal("return 'filesystem'")
			control.finishCheckpoint({ checkpointId = "secured" })
			expect(applying:await()).to.equal(true)
			expect(control.text).to.equal("return 'filesystem'")
			expect(session:getSyncResults()[1].status).to.equal("Conflicted")
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should retain the latest draft when checkpoint storage fails or typing overtakes storage", function()
		for _, newer in { false, true } do
			local session, instance, control = sourceRecoverySession()
			local success, problem = pcall(function()
				session:__rememberLocalProperty(instance, "Source")
				session.__localPropertyEdits = {}
				local applying = session:__applyPatch(sourcePublication("return 'filesystem'"), 1)
				if newer then
					control.text = "return 'newest draft'"
					session:__rememberLocalProperty(instance, "Source")
					control.finishCheckpoint({ checkpointId = "previous draft only" })
				else
					control.failCheckpoint("The recovery cache is full.")
				end
				expect(applying:await()).to.equal(false)
				expect(control.text).to.equal(if newer then "return 'newest draft'" else "return 'draft'")
				expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			end)
			session:stop()
			instance:Destroy()
			assert(success, problem)
		end
	end)

	it("should preserve integrated document intent over one thousand replayable acknowledgement schedules", function()
		local Source = require(script.Parent.Reconciler.setSource)
		for seed = 1, 1000 do
			local control = { cursor = 0, text = "return 'initial'", requests = {}, jobs = {} }
			local changed
			local setter = Source({
				ConnectChanged = function(_, callback)
					changed = callback
					return { Disconnect = function() end }
				end,
				GetEditorSource = function()
					return control.text
				end,
				Spawn = function(callback)
					table.insert(control.jobs, callback)
				end,
				UpdateSourceAsync = function(_, callback)
					local value = callback(control.text)
					if value ~= nil and value ~= control.text then
						control.text = value
						changed()
					end
				end,
			})
			local session = ServeSession.new({
				twoWaySync = true,
				apiContext = {
					getMessageCursor = function()
						return control.cursor
					end,
					disconnect = function() end,
					write = function(_, _, cursor, _, operation, predecessor, baselines)
						return Promise.new(function(resolve)
							table.insert(control.requests, {
								Operation = operation,
								Resolve = resolve,
								Cursor = cursor,
								Predecessor = predecessor,
								Baselines = baselines,
							})
						end)
					end,
				},
			})
			session.__changeBatcher.__renderSteppedConnection:Disconnect()
			session.__deferredChanges = nil
			session.__instanceMap.sourceSetter = setter
			session.__instanceMap.__isGameRunning = function()
				return false
			end
			session.__readEditorSource = function()
				return control.text
			end
			local instance = Instance.new("ModuleScript")
			session.__instanceMap:insert("EDITED", instance)
			local function edit(text)
				control.text = text
				changed()
			end
			local function write()
				local captured = session:__takeLocalPropertyEdits({ [instance] = { Source = true } })
				return session:__writePatch({
					added = {},
					removed = {},
					updated = {
						{ id = "EDITED", changedProperties = { Source = { String = control.text } } },
					},
				}, control.cursor, captured)
			end
			local function acknowledge(index)
				local response = writeResult()
				response.messageCursor = index
				response.operationId = control.requests[index].Operation
				response.acknowledgement = `proof-{index}`
				control.requests[index].Resolve(response)
			end
			local function publish(text, operation, cursor)
				local applying = session:__applyPatch(sourcePublication(text, operation), cursor)
				while #control.jobs > 0 do
					table.remove(control.jobs, 1)()
				end
				if applying ~= nil then
					assert(applying:await(), `Source publication failed at seed {seed}.`)
				end
			end
			local randomState = seed
			local function choose()
				randomState = randomState * 48271 % 2147483647
				return randomState
			end
			local success, problem = pcall(function()
				publish(control.text, nil, 0)
				for generation = 1, 8 do
					local firstIndex, latestIndex = generation * 2 - 1, generation * 2
					local first = `return 'first {seed}/{generation}'`
					local latest = `return 'latest {seed}/{generation}'`
					edit(first)
					local firstWrite = write()
					local operation = control.requests[firstIndex].Operation
					local earlyAcknowledgement = choose() % 2 == 0
					if earlyAcknowledgement then
						acknowledge(firstIndex)
					end
					edit(latest)
					if choose() % 2 == 0 then
						edit(first)
						edit(latest)
					end
					local secondWrite = write()
					publish(first, operation, firstIndex)
					assert(
						control.text == latest,
						`An earlier echo rewound typing at seed {seed}, generation {generation}.`
					)
					if not earlyAcknowledgement then
						acknowledge(firstIndex)
					end
					assert(firstWrite:await(), `The first write failed at seed {seed}.`)
					expect(control.requests[firstIndex].Predecessor).to.equal(
						if firstIndex == 1 then nil else `proof-{firstIndex - 1}`
					)
					expect(control.requests[latestIndex].Predecessor).to.equal(`proof-{firstIndex}`)
					expect(control.requests[firstIndex].Cursor).to.equal((generation - 1) * 2)
					expect(control.requests[firstIndex].Baselines.EDITED).to.equal((generation - 1) * 2)
					expect(control.requests[latestIndex].Baselines.EDITED).to.equal((generation - 1) * 2)
					for _ = 1, choose() % 4 do
						publish(first, operation, firstIndex)
					end
					acknowledge(latestIndex)
					assert(secondWrite:await(), `The latest write failed at seed {seed}.`)
					control.cursor = latestIndex
					session:__releaseLocalProperties(latestIndex)
					publish(latest, control.requests[latestIndex].Operation, latestIndex)
					for _ = 1, 1 + choose() % 4 do
						publish(first, operation, firstIndex)
					end
					assert(
						control.text == latest,
						`A settled earlier echo rewound typing at seed {seed}, generation {generation}.`
					)
					assert(session.__sourceDrafts.EDITED == nil, `A saved draft remained at seed {seed}.`)
					assert(next(session.__remoteSources) == nil, `A Source operation remained at seed {seed}.`)
				end
			end)
			session:stop()
			instance:Destroy()
			assert(success, problem)
			if seed % 32 == 0 then
				task.wait()
			end
		end
	end)

	it("should not replay buffered Source behind a newer queued filesystem publication", function()
		local session, instance, control = sourceRecoverySession()
		local release
		session.__pendingSourcePatch = Promise.new(function(resolve)
			release = resolve
		end)
		session.__localPropertyEdits.EDITED = {
			Source = {
				cursor = 1,
				serverCursor = 1,
				serverInstance = instance,
				serverValue = { String = "return 'disk1'" },
			},
		}
		local success, problem = pcall(function()
			local newer = session:__applyPatch(sourcePublication("return 'disk2'"), 2)
			local replay = session:__releaseLocalProperties(1)
			release()
			expect(newer:getStatus()).to.equal(Promise.Status.Resolved)
			if replay ~= nil then
				expect(replay:getStatus()).to.equal(Promise.Status.Resolved)
			end
			expect(control.text).to.equal("return 'disk2'")
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should not classify matching historical text without provenance as an echo", function()
		local session, instance, control = sourceRecoverySession()
		local success, problem = pcall(function()
			session.__sourceHistory:begin(
				"old",
				{ EDITED = { Instance = instance, Revision = 0, Source = "return 'historical'" } }
			)
			session.__sourceHistory:finish("old")
			session:__rememberLocalProperty(instance, "Source")
			session.__localPropertyEdits = {}
			local applying = session:__applyPatch(sourcePublication("return 'historical'"), 1)
			expect(#control.checkpoints).to.equal(1)
			control.finishCheckpoint({ checkpointId = "secured" })
			expect(applying:await()).to.equal(true)
			expect(control.text).to.equal("return 'historical'")
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should restore a checkpoint only as an explicit guarded local editor change", function()
		local session, instance, control = sourceRecoverySession()
		session.__apiContext.readSourceCheckpoint = function()
			return Promise.resolve({
				sessionId = "session",
				checkpoint = { id = "EDITED", source = "return 'recovered'" },
			})
		end
		session.__updateEditorSource = function(self, _instance, callback)
			local value = callback()
			if value ~= nil then
				control.text = value
				self:__rememberLocalProperty(instance, "Source")
			end
		end
		local success, problem = pcall(function()
			local restored, result = session:restoreSourceCheckpoint("checkpoint", "EDITED", control.text):await()
			expect(restored).to.equal(true)
			expect(result.editorRestored).to.equal(true)
			expect(control.text).to.equal("return 'recovered'")
			expect(session.__sourceDrafts.EDITED.Source).to.equal("return 'recovered'")
			expect(#control.requests).to.equal(0)
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should refuse recovery after an edit and exact reversion while reading its checkpoint", function()
		local session, instance, control = sourceRecoverySession()
		local resolveRead
		session.__apiContext.readSourceCheckpoint = function()
			return Promise.new(function(resolve)
				resolveRead = resolve
			end)
		end
		session.__updateEditorSource = function(_, _instance, callback)
			expect(callback()).to.equal(nil)
		end
		local success, problem = pcall(function()
			local restoring = session:restoreSourceCheckpoint("checkpoint", "EDITED", control.text)
			local previous = control.text
			control.text = "new typing"
			session:__rememberLocalProperty(instance, "Source")
			control.text = previous
			session:__rememberLocalProperty(instance, "Source")
			resolveRead({ sessionId = "session", checkpoint = { id = "EDITED", source = "return 'recovered'" } })
			expect(restoring:await()).to.equal(false)
			expect(control.text).to.equal(previous)
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should never replay a checkpoint onto a different tracked script identity", function()
		local session, instance, control = sourceRecoverySession()
		session.__apiContext.readSourceCheckpoint = function()
			return Promise.resolve({ sessionId = "session", checkpoint = { id = "OTHER", source = "wrong draft" } })
		end
		session.__updateEditorSource = function()
			error("A different identity must not reach the editor.")
		end
		local success, problem = pcall(function()
			expect(session:restoreSourceCheckpoint("checkpoint", "EDITED", control.text):await()).to.equal(false)
			expect(control.text).to.equal("return 'draft'")
		end)
		session:stop()
		instance:Destroy()
		assert(success, problem)
	end)

	it("should finish shutdown before notifying reentrant or throwing status callbacks", function()
		for _, reenter in { false, true } do
			local session, part = propertySession("Part")
			session.__status = ServeSession.Status.Connected
			local disconnected = false
			session.__apiContext.disconnect = function()
				disconnected = true
			end
			local notifications = 0
			local cleanedUp = false
			session:onStatusChanged(function(status)
				notifications += 1
				cleanedUp = status == ServeSession.Status.Disconnected
					and disconnected
					and session.__changeBatcher.__stopped
					and session.__instanceMap:size() == 0
					and session.__localPropertyEdits == nil
					and #session.__connections == 0
				if reenter and notifications == 1 then
					session:stop()
				else
					error("The disconnected status callback failed.")
				end
			end)
			local success, problem = pcall(function()
				expect(pcall(function()
					session:stop()
				end)).to.equal(true)
				expect(cleanedUp).to.equal(true)
				expect(notifications).to.equal(1)
				session:stop()
				expect(notifications).to.equal(1)
			end)
			session:onStatusChanged(nil)
			session:stop()
			part:Destroy()
			assert(success, problem)
		end
	end)

	it("should stop tracking and queued writes after a fatal acknowledgement without replaying old values", function()
		local ApiContext = require(script.Parent.ApiContext)
		local Http = require(script.Parent.Parent.Packages.Http)
		local originalPost = Http.post
		for _, body in { string.char(0x81), Http.msgpackEncode({ sessionId = "session", messageCursor = 100 }) } do
			local session, part = propertySession("Part")
			local context = ApiContext.new("http://localhost")
			context.__sessionId = "session"
			context:setMessageCursor(0)
			session.__apiContext = context
			session.__status = ServeSession.Status.Connected
			local requests = 0
			local settle
			Http.post = function()
				requests += 1
				if requests == 1 then
					return Promise.new(function(resolve)
						settle = resolve
					end)
				end
				return Promise.resolve({ code = 200, body = body })
			end
			local success, problem = pcall(function()
				part.Transparency = 0.4
				session:__queueChange(part, "Transparency")
				local first = session:flushChanges()
				part.Transparency = 0.7
				local latest = part.Transparency
				session:__queueChange(part, "Transparency")
				local second = session:flushChanges()
				session:__applyPatch({
					added = {},
					removed = {},
					updated = { { id = "EDITED", changedProperties = { Transparency = { Float32 = 0.2 } } } },
				})
				settle({ code = 200, body = body })
				first:await()
				second:await()
				expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
				expect(session.__changeBatcher.__stopped).to.equal(true)
				expect(session.__instanceMap:size()).to.equal(0)
				expect(session.__localPropertyEdits).to.equal(nil)
				expect(part.Transparency).to.equal(latest)
				expect(requests).to.equal(1)
				for _, result in session:getSyncResults() do
					expect(result.status).to.equal("Failed")
				end
			end)
			Http.post = originalPost
			session:stop()
			part:Destroy()
			assert(success, problem)
		end
	end)

	it("should stop after acknowledgement processing fails without treating it as a transport failure", function()
		local session, part, control = propertySession("Part")
		session.__status = ServeSession.Status.Connected
		local transportFailures = 0
		session.__untrackRejectedAdditions = function(_self, _patch, result)
			if result ~= nil then
				error("Acknowledgement processing failed.")
			end
			transportFailures += 1
		end
		local success, problem = pcall(function()
			part.Transparency = 0.7
			local latest = part.Transparency
			session:__queueChange(part, "Transparency")
			local first = session:flushChanges()
			local second = session:__writePatch({ added = {}, removed = {}, updated = {} }, 0, {})
			session:__applyPatch({
				added = {},
				removed = {},
				updated = { { id = "EDITED", changedProperties = { Transparency = { Float32 = 0.2 } } } },
			})
			local result = writeResult()
			result.messageCursor = 100
			control.settle[1](result)
			first:await()
			second:await()
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(session.__changeBatcher.__stopped).to.equal(true)
			expect(session.__localPropertyEdits).to.equal(nil)
			expect(part.Transparency).to.equal(latest)
			expect(#control.requests).to.equal(1)
			expect(transportFailures).to.equal(1)
			for _, record in session:getSyncResults() do
				expect(record.status).to.equal("Failed")
			end
		end)
		session:stop()
		part:Destroy()
		assert(success, problem)
	end)

	it("should retain local values until all acknowledgement processing succeeds", function()
		for _, caughtUp in { false, true } do
			for _, callbackFails in { false, true } do
				local session, part, control = propertySession("Part")
				session.__status = ServeSession.Status.Connected
				if callbackFails then
					session:setSyncResultCallback(function(status)
						if status == "Saved" then
							error("The result callback failed.")
						end
					end)
				else
					session.__untrackRejectedAdditions = function(_self, _patch, result)
						if result ~= nil then
							error("Acknowledgement processing failed.")
						end
					end
				end
				local success, problem = pcall(function()
					part.Transparency = 0.7
					local latest = part.Transparency
					session:__queueChange(part, "Transparency")
					local first = session:flushChanges()
					local second = session:__writePatch({ added = {}, removed = {}, updated = {} }, 0, {})
					session:__applyPatch({
						added = {},
						removed = {},
						updated = { { id = "EDITED", changedProperties = { Transparency = { Float32 = 0.2 } } } },
					})
					control.cursor = if caughtUp then 100 else 0
					local result = writeResult()
					result.messageCursor = 100
					control.settle[1](result)
					first:await()
					second:await()
					expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
					expect(part.Transparency).to.equal(latest)
					expect(#control.requests).to.equal(1)
					for _, record in session:getSyncResults() do
						expect(record.status).to.equal("Failed")
					end
				end)
				session:stop()
				part:Destroy()
				assert(success, problem)
			end
		end
	end)

	it("should keep structural failures out of replacement fallback", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		local PatchSet = require(script.Parent.PatchSet)
		local Settings = require(script.Parent.Settings)
		local applyPatch = require(script.Parent.Reconciler.applyPatch)
		local original = Settings:get("enableSyncFallback")
		local instanceMap = InstanceMap.new()
		local instance = Instance.new("Folder")
		instanceMap:insert("INSTANCE", instance)
		local success, err = pcall(function()
			Settings:set("enableSyncFallback", true)
			local session = sessionState({
				__instanceMap = instanceMap,
				__precommitCallbacks = {},
				__postcommitCallbacks = {},
				__reconciler = {
					applyPatch = function(_self, patch)
						return applyPatch(instanceMap, patch)
					end,
				},
				__replaceInstances = function(_self, ids)
					expect(#ids).to.equal(0)
					return true, PatchSet.newEmpty()
				end,
			}, ServeSession)
			local patch = PatchSet.newEmpty()
			patch.updated = { { id = "INSTANCE", changedParent = "MISSING", changedProperties = {} } }
			session:__applyPatch(patch)
			expect(instanceMap.fromIds.INSTANCE).to.equal(instance)
		end)
		Settings:set("enableSyncFallback", original)
		instanceMap:stop()
		instance:Destroy()
		assert(success, err)
	end)

	it("should restore fallback replacements and retained children to their final authoritative parents", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		local PatchSet = require(script.Parent.PatchSet)
		local createPatchSet = require(script.Parent.ChangeBatcher.createPatchSet)
		local root = Instance.new("Folder")
		local original = Instance.new("Folder")
		original.Name = "Parent"
		original.Parent = root
		local child = Instance.new("Folder")
		child.Parent = original
		local unmanaged = Instance.new("Folder")
		local reports = {}
		local instanceMap = InstanceMap.new(function(_, property)
			table.insert(reports, property)
		end)
		instanceMap.__isGameRunning = function()
			return false
		end
		instanceMap:insert("ROOT", root)
		instanceMap:insert("PARENT", original)
		instanceMap:insert("CHILD", child)

		local model = Instance.new("Folder")
		local pointer = Instance.new("ObjectValue")
		pointer.Name = "PARENT"
		pointer.Parent = model
		local serializedParent = Instance.new("Folder")
		serializedParent.Name = "Parent"
		serializedParent.Parent = pointer
		pointer.Value = serializedParent
		local contents = game:GetService("SerializationService"):SerializeInstancesAsync({ model })
		local session = sessionState({
			__instanceMap = instanceMap,
			__apiContext = {
				serialize = function()
					return Promise.resolve({ modelContents = contents })
				end,
				refPatch = function()
					return Promise.resolve({ patch = PatchSet.newEmpty() })
				end,
			},
			__reconciler = {
				applyPatch = function(_self, patch)
					return require(script.Parent.Reconciler.applyPatch)(instanceMap, patch)
				end,
			},
		}, ServeSession)
		local success, unapplied = session:__replaceInstances({ "PARENT" })
		expect(success).to.equal(true)
		for _, property in reports do
			expect(property == "Parent").to.equal(false)
		end
		expect(PatchSet.isEmpty(unapplied)).to.equal(true)
		local replacement = instanceMap.fromIds.PARENT
		expect(replacement ~= original).to.equal(true)
		expect(replacement.Parent).to.equal(root)
		expect(child.Parent).to.equal(replacement)
		expect(instanceMap.knownParents[replacement]).to.equal(root)
		expect(instanceMap.observedParents[replacement]).to.equal(root)
		expect(instanceMap.knownParents[child]).to.equal(replacement)
		expect(instanceMap.observedParents[child]).to.equal(replacement)
		expect(instanceMap.pausedUpdateInstances[child]).to.equal(true)
		instanceMap:unpauseAllInstances()
		replacement.Parent = unmanaged
		expect(PatchSet.isEmpty(createPatchSet(instanceMap, { [replacement] = { Parent = true } }))).to.equal(true)
		expect(replacement.Parent).to.equal(root)
		child.Parent = unmanaged
		expect(PatchSet.isEmpty(createPatchSet(instanceMap, { [child] = { Parent = true } }))).to.equal(true)
		expect(child.Parent).to.equal(replacement)
		expect(instanceMap.fromIds.CHILD).to.equal(child)
		instanceMap:stop()
		root:Destroy()
		original:Destroy()
		unmanaged:Destroy()
		model:Destroy()
	end)

	it("should reject initial changes with the current name parent and managed refs", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		local PatchSet = require(script.Parent.PatchSet)
		local instanceMap = InstanceMap.new()
		local parent = Instance.new("Folder")
		local target = Instance.new("Folder")
		local instance = Instance.new("ObjectValue")
		instance.Name = "StudioName"
		instance.Parent = parent
		instance.Value = target
		instanceMap:insert("INSTANCE", instance)
		instanceMap:insert("PARENT", parent)
		instanceMap:insert("TARGET", target)
		local catchUp = PatchSet.newEmpty()
		catchUp.updated = {
			{
				id = "INSTANCE",
				changedName = "DiskName",
				changedParent = "OTHER",
				changedProperties = { Value = { Ref = "OTHER" } },
			},
		}
		local sent
		local session = sessionState({
			__instanceMap = instanceMap,
			__twoWaySync = true,
			__updateLoadingText = function() end,
			__userConfirmCallback = function()
				return "Reject"
			end,
			__reconciler = {
				hydrate = function() end,
				diff = function()
					return true, catchUp
				end,
			},
			__apiContext = {
				read = function()
					return Promise.resolve({ messageCursor = 1, instances = {} })
				end,
				setMessageCursor = function() end,
				write = function(_self, patch, _cursor, authority)
					sent = patch
					expect(authority).to.equal("Studio")
					return Promise.resolve({})
				end,
			},
		}, ServeSession)
		expect(session:__initialSync({ rootInstanceId = "ROOT" }):await()).to.equal(true)
		expect(sent.updated[1].changedName).to.equal("StudioName")
		expect(sent.updated[1].changedParent).to.equal("PARENT")
		expect(sent.updated[1].changedProperties.Value.Ref).to.equal("TARGET")
		-- A class-only rejection has nothing representable to encode.
		catchUp.updated = { { id = "INSTANCE", changedClassName = "Folder", changedProperties = {} } }
		expect(session:__initialSync({ rootInstanceId = "ROOT" }):await()).to.equal(true)
		expect(#sent.updated).to.equal(0)
		instanceMap:stop()
		parent:Destroy()
		target:Destroy()
	end)

	it("should report transport failures to the notification callback", function()
		local session = sessionState({
			__apiContext = {
				write = function()
					return Promise.reject("Connection lost")
				end,
			},
			__syncResults = {},
		}, ServeSession)
		local notification = nil
		session:setSyncResultCallback(function(status, result)
			notification = { status = status, result = result }
		end)
		session:__writePatch({ updated = {}, added = {}, removed = {} }):await()
		local results = session:getSyncResults()
		expect(#results).to.equal(1)
		expect(results[1].status).to.equal("Failed")
		expect(notification.status).to.equal("Failed")
		expect(notification.result.errors[1].message).to.equal("Connection lost")
	end)

	it("should keep one record per write that moves from pending to its outcome", function()
		local resolveWrite = nil
		local session = sessionState({
			__apiContext = {
				write = function()
					return Promise.new(function(resolve)
						resolveWrite = resolve
					end)
				end,
			},
			__syncResults = {},
		}, ServeSession)
		local patch = { updated = {}, added = {}, removed = {} }
		local write = session:__writePatch(patch)

		local pending = session:getSyncResults()
		expect(#pending).to.equal(1)
		expect(pending[1].status).to.equal("Pending")
		expect(pending[1].patch).to.equal(patch)

		resolveWrite({
			errors = {},
			accepted = { removed_instances = {}, added_instances = {}, updated_instances = {} },
			conflicts = { removed_instances = {}, added_instances = {}, updated_instances = {} },
		})
		write:await()

		local finished = session:getSyncResults()
		expect(#finished).to.equal(1)
		expect(finished[1].status).to.equal("Saved")
		expect(finished[1].patch).to.equal(patch)
		-- Callers get copies, so earlier snapshots don't change underneath them.
		expect(pending[1].status).to.equal("Pending")
	end)

	-- A write still waiting when the session stops is never sent, and it's
	-- reported as failed.
	it("should not send writes queued when the session stops", function()
		local settle = nil
		local started = {}
		local session = sessionState({
			__apiContext = {
				write = function(_self, patch)
					table.insert(started, patch.name)
					return Promise.new(function(resolve)
						settle = resolve
					end)
				end,
			},
			__syncResults = {},
		}, ServeSession)
		local first = session:__writePatch({ name = "first", updated = {}, added = {}, removed = {} })
		local second = session:__writePatch({ name = "second", updated = {}, added = {}, removed = {} })
		session.__status = ServeSession.Status.Disconnected
		settle(writeResult())
		first:await()
		second:await()
		expect(table.concat(started, ",")).to.equal("first")
		expect(session:getSyncResults()[2].status).to.equal("Failed")
	end)

	-- The server can announce a saved deletion before answering the write.
	-- An undo made before the announcement survives it and is created again,
	-- while a deletion that wasn't undone is applied as usual.
	it("should keep undone deletions when their announcement comes first", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		for _, undone in { false, true } do
			local instanceMap = InstanceMap.new()
			local parent = Instance.new("Folder")
			local deleted = Instance.new("Folder")
			local child = Instance.new("Folder")
			child.Parent = deleted
			instanceMap:insert("PARENT", parent)
			instanceMap:insert("DELETED", deleted)
			instanceMap:insert("CHILD", child)
			local queued = {}
			local session = sessionState({
				__instanceMap = instanceMap,
				__pendingDeletions = { DELETED = deleted },
				__markBatchStart = function() end,
				__changeBatcher = {
					addChild = function(_self, instance)
						table.insert(queued, instance)
					end,
				},
			}, ServeSession)
			if undone then
				deleted.Parent = parent
			end
			local patch = { removed = { "DELETED" }, added = {}, updated = {} }

			session:__keepUndoneDeletions(patch)

			expect(#patch.removed).to.equal(if undone then 0 else 1)
			expect(instanceMap.fromIds.DELETED == nil).to.equal(undone)
			expect(instanceMap.fromInstances[child] == nil).to.equal(undone)
			expect(#queued).to.equal(if undone then 1 else 0)
			instanceMap:stop()
			parent:Destroy()
			deleted:Destroy()
		end
	end)

	-- A later write waits for the one before it, whether that one succeeds or
	-- fails, so an edit never reaches the server before the write that made
	-- its instance.
	it("should send writes one after another", function()
		for _, firstFails in { false, true } do
			local settle = {}
			local started = {}
			local session = sessionState({
				__apiContext = {
					write = function(_self, patch)
						table.insert(started, patch.name)
						return Promise.new(function(resolve, reject)
							settle[patch.name] = if firstFails and patch.name == "first" then reject else resolve
						end)
					end,
				},
				__syncResults = {},
			}, ServeSession)
			local first = session:__writePatch({ name = "first", updated = {}, added = {}, removed = {} })
			local second = session:__writePatch({ name = "second", updated = {}, added = {}, removed = {} })
			expect(table.concat(started, ",")).to.equal("first")

			settle.first(writeResult())
			first:await()
			expect(table.concat(started, ",")).to.equal("first,second")
			settle.second(writeResult())
			second:await()
		end
	end)

	it("should read a batched attribute map only when the batch is encoded", function()
		local session, part, control = propertySession("Part")
		local RbxDom = require(script.Parent.Parent.Packages.RbxDom)
		local original = RbxDom.findCanonicalPropertyDescriptor
		local reads = 0
		RbxDom.findCanonicalPropertyDescriptor = function(className, propertyName)
			local descriptor = original(className, propertyName)
			if descriptor and propertyName == "Attributes" then
				local observed = table.clone(descriptor)
				observed.read = function(_self, instance)
					reads += 1
					return descriptor:read(instance)
				end
				return observed
			end
			return descriptor
		end
		local success, problem = pcall(function()
			for index = 1, 64 do
				part:SetAttribute(`Key{index}`, index)
				session:__queueChange(part, "Attributes")
			end
			expect(reads).to.equal(0)
			local write = session:flushChanges()
			expect(reads).to.equal(1)
			local encoded = control.requests[1].updated[1].changedProperties.Attributes.Attributes
			expect(encoded.Key64 ~= nil).to.equal(true)
			local result = writeResult()
			result.messageCursor = 0
			control.settle[1](result)
			write:await()
		end)
		RbxDom.findCanonicalPropertyDescriptor = original
		session:stop()
		part:Destroy()
		assert(success, problem)
	end)

	for _, mixed in { false, true } do
		it(`should release an omitted property's guard in a mixed batch "{mixed}"`, function()
			local session, part, control = propertySession("Part")
			local RbxDom = require(script.Parent.Parent.Packages.RbxDom)
			local original = RbxDom.findCanonicalPropertyDescriptor
			part.Transparency = 0.5
			session:__queueChange(part, "Transparency")
			if mixed then
				part.Size = Vector3.new(5, 2, 2)
				session:__queueChange(part, "Size")
			end
			RbxDom.findCanonicalPropertyDescriptor = function(className, propertyName)
				local descriptor = original(className, propertyName)
				if descriptor and propertyName == "Transparency" then
					local unreadable = table.clone(descriptor)
					unreadable.read = function()
						return false, "The property cannot be read."
					end
					return unreadable
				end
				return descriptor
			end
			local success, problem = pcall(function()
				local write = session:flushChanges()
				local pending = session.__localPropertyEdits.EDITED
				expect(pending == nil or pending.Transparency == nil).to.equal(true)
				if mixed then
					expect(pending.Size ~= nil).to.equal(true)
					local result = writeResult()
					result.messageCursor = 0
					control.settle[1](result)
					write:await()
				else
					expect(write).to.equal(nil)
					expect(#control.requests).to.equal(0)
				end
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			RbxDom.findCanonicalPropertyDescriptor = original
			session:stop()
			part:Destroy()
			assert(success, problem)
		end)
	end

	for _, names in { { "EmitterSize", "RollOffMinDistance" }, { "RollOffMinDistance", "EmitterSize" } } do
		it(`should protect "{names[1]}" edits from delayed "{names[2]}" echoes`, function()
			for _, stage in { "pending", "queued", "inflight", "answered" } do
				local session, sound, control = propertySession("Sound")
				local function edit(value)
					sound[names[1]] = value
					session:__queueChange(sound, names[1])
				end
				local function echo(value)
					session:__applyPatch({
						added = {},
						removed = {},
						updated = { { id = "EDITED", changedProperties = { [names[2]] = { Float32 = value } } } },
					})
				end
				local function answer(index, revision)
					local result = writeResult()
					result.messageCursor = revision
					control.settle[index](result)
				end
				local success, problem = pcall(function()
					edit(4)
					local first = session:flushChanges()
					edit(5)
					local second = if stage ~= "pending" then session:flushChanges() else nil
					if stage == "inflight" or stage == "answered" then
						answer(1, 1)
						first:await()
					end
					if stage == "answered" then
						answer(2, 2)
						second:await()
					end
					echo(4)
					expect(sound.RollOffMinDistance).to.equal(5)
					control.cursor = 1
					session:__releaseLocalProperties(control.cursor)
					if stage == "pending" or stage == "queued" then
						answer(1, 1)
						first:await()
					end
					if second == nil then
						second = session:flushChanges()
					end
					local encoded = control.requests[2].updated[1].changedProperties
					expect(encoded.RollOffMinDistance.Float32).to.equal(5)
					expect(encoded.EmitterSize).to.equal(nil)
					if stage ~= "answered" then
						answer(2, 2)
						second:await()
					end
					echo(5)
					control.cursor = 2
					session:__releaseLocalProperties(control.cursor)
					expect(sound.RollOffMinDistance).to.equal(5)
					expect(next(session.__localPropertyEdits)).to.equal(nil)
					echo(6)
					expect(sound.RollOffMinDistance).to.equal(6)
				end)
				session:stop()
				sound:Destroy()
				assert(success, `{stage} {tostring(problem)}`)
			end
		end)
	end

	it("should use one generation and encoding for canonical and alias signals in a batch", function()
		local session, sound, control = propertySession("Sound")
		local RbxDom = require(script.Parent.Parent.Packages.RbxDom)
		local original = RbxDom.findCanonicalPropertyDescriptor
		local reads = 0
		RbxDom.findCanonicalPropertyDescriptor = function(className, propertyName)
			local descriptor = original(className, propertyName)
			if descriptor and descriptor.name == "RollOffMinDistance" then
				local observed = table.clone(descriptor)
				observed.read = function(_self, instance)
					reads += 1
					return descriptor:read(instance)
				end
				return observed
			end
			return descriptor
		end
		local success, problem = pcall(function()
			sound.EmitterSize = 4
			session:__queueChange(sound, "EmitterSize")
			sound.RollOffMinDistance = 5
			session:__queueChange(sound, "RollOffMinDistance")
			local pending = session.__localPropertyEdits.EDITED
			expect(pending.EmitterSize).to.equal(nil)
			expect(pending.RollOffMinDistance ~= nil).to.equal(true)
			expect(reads).to.equal(0)
			local write = session:flushChanges()
			expect(reads).to.equal(1)
			local encoded = control.requests[1].updated[1].changedProperties
			expect(encoded.EmitterSize).to.equal(nil)
			expect(encoded.RollOffMinDistance.Float32).to.equal(5)
			local result = writeResult()
			result.messageCursor = 0
			control.settle[1](result)
			write:await()
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
		RbxDom.findCanonicalPropertyDescriptor = original
		session:stop()
		sound:Destroy()
		assert(success, problem)
	end)

	it("should release a canonical guard when an alias signal is omitted during encoding", function()
		local session, sound, control = propertySession("Sound")
		local RbxDom = require(script.Parent.Parent.Packages.RbxDom)
		local original = RbxDom.findCanonicalPropertyDescriptor
		sound.EmitterSize = 5
		session:__queueChange(sound, "EmitterSize")
		RbxDom.findCanonicalPropertyDescriptor = function(className, propertyName)
			local descriptor = original(className, propertyName)
			if descriptor and descriptor.name == "RollOffMinDistance" then
				local unreadable = table.clone(descriptor)
				unreadable.read = function()
					return false, "The property cannot be read."
				end
				return unreadable
			end
			return descriptor
		end
		local success, problem = pcall(function()
			expect(session:flushChanges()).to.equal(nil)
			expect(#control.requests).to.equal(0)
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
		RbxDom.findCanonicalPropertyDescriptor = original
		session:stop()
		sound:Destroy()
		assert(success, problem)
	end)

	it("should preserve newer script text across delayed Source echoes at every write stage", function()
		for _, className in { "ModuleScript", "Script", "LocalScript" } do
			for _, stage in { "pending", "queued", "inflight", "answered" } do
				local session, instance, control = propertySession(className)
				session.__applyPatch = function(_, patch)
					for _, update in patch.updated do
						if update.changedProperties.Source then
							instance.Source = update.changedProperties.Source.String
						end
					end
				end
				local function edit(text)
					instance.Source = text
					session:__queueChange(instance, "Source")
				end
				local function filter(text)
					return session:__protectLocalProperties({
						added = {},
						removed = {},
						updated = { { id = "EDITED", changedProperties = { Source = { String = text } } } },
					})
				end
				local function answer(index, revision)
					local result = writeResult()
					result.messageCursor = revision
					control.settle[index](result)
				end
				local success, problem = pcall(function()
					edit("return 'a'")
					local first = session:flushChanges()
					edit("return 'ab'")
					local second = if stage ~= "pending" then session:flushChanges() else nil
					if stage == "inflight" or stage == "answered" then
						answer(1, 1)
						first:await()
					end
					if stage == "answered" then
						answer(2, 2)
						second:await()
					end
					expect(#filter("return 'a'").updated).to.equal(0)
					control.cursor = 1
					session:__releaseLocalProperties(control.cursor)
					if stage == "pending" or stage == "queued" then
						answer(1, 1)
						first:await()
					end
					if second == nil then
						second = session:flushChanges()
					end
					expect(control.requests[2].updated[1].changedProperties.Source.String).to.equal("return 'ab'")
					if stage ~= "answered" then
						answer(2, 2)
						second:await()
					end
					filter("return 'ab'")
					control.cursor = 2
					session:__releaseLocalProperties(control.cursor)
					local external = filter("return 'filesystem'")
					expect(external.updated[1].changedProperties.Source.String).to.equal("return 'filesystem'")
				end)
				session:stop()
				instance:Destroy()
				assert(success, `{className} {stage} {tostring(problem)}`)
			end
		end
	end)

	it("should preserve only failed properties without an authoritative conflict", function()
		for _, errorId in { "EDITED", "OTHER" } do
			for _, property in { false, "Source", "Transparency" } do
				for _, conflictId in { false, "EDITED", "OTHER" } do
					for _, removed in { false, true } do
						local result = writeResult({
							{
								id = errorId,
								property = if property then property else nil,
								kind = "sourceSerializationDeferred",
								message = "The write could not be serialized",
							},
						})
						if conflictId then
							if removed then
								table.insert(result.conflicts.removed_instances, conflictId)
							else
								table.insert(result.conflicts.updated_instances, {
									id = conflictId,
									changed_properties = { Source = { String = "return 'disk'" } },
								})
							end
						end
						local expected = errorId == "EDITED" and property == "Source" and conflictId ~= "EDITED"
						expect(ServeSession.shouldPreserveFailedProperty(result, "EDITED", "Source")).to.equal(expected)
						expect(ServeSession.shouldPreserveFailedProperty(result, "EDITED", "Source")).to.equal(expected)
						result.errors[1].kind = nil
						expect(ServeSession.shouldPreserveFailedProperty(result, "EDITED", "Source")).to.equal(false)
					end
				end
			end
		end
	end)

	it("should preserve a deferred Source independently of another conflicted property", function()
		local result = writeResult({
			{
				id = "EDITED",
				kind = "sourceSerializationDeferred",
				property = "Source",
				message = "Unsaved draft",
			},
		})
		result.conflicts.updated_instances =
			{ { id = "EDITED", changed_properties = { Archivable = { Bool = true } } } }
		expect(ServeSession.shouldPreserveFailedProperty(result, "EDITED", "Source")).to.equal(true)
		result.conflicts.updated_instances[1].changed_properties.Source = { String = "return 'disk'" }
		expect(ServeSession.shouldPreserveFailedProperty(result, "EDITED", "Source")).to.equal(false)
	end)

	it("should retain an unsaved script draft after a serialization error without hiding later disk edits", function()
		for _, className in { "ModuleScript", "Script", "LocalScript" } do
			for _, responseFirst in { false, true } do
				local session, instance, control = propertySession(className)
				session.__applyPatch = function(_, patch)
					for _, update in patch.updated do
						if update.changedProperties.Source then
							instance.Source = update.changedProperties.Source.String
						end
					end
				end
				local success, problem = pcall(function()
					instance.Source = "local value ="
					session:__queueChange(instance, "Source")
					local write = session:flushChanges()
					local result = writeResult({
						{
							id = "EDITED",
							kind = "sourceSerializationDeferred",
							property = "Source",
							message = "Cannot safely reverse imports in incomplete source",
						},
					})
					result.messageCursor = 1
					if responseFirst then
						control.settle[1](result)
						write:await()
					end
					local filtered = session:__protectLocalProperties({
						added = {},
						removed = {},
						updated = {
							{ id = "EDITED", changedProperties = { Source = { String = "return 'previous save'" } } },
						},
					})
					expect(#filtered.updated).to.equal(0)
					control.cursor = 1
					session:__releaseLocalProperties(control.cursor)
					if not responseFirst then
						control.settle[1](result)
						write:await()
					end
					expect(instance.Source).to.equal("local value =")
					expect(session:getSyncResults()[1].status).to.equal("Failed")
					expect(next(session.__localPropertyEdits)).to.equal(nil)
					local external = session:__protectLocalProperties({
						added = {},
						removed = {},
						updated = {
							{ id = "EDITED", changedProperties = { Source = { String = "return 'external edit'" } } },
						},
					})
					expect(external.updated[1].changedProperties.Source.String).to.equal("return 'external edit'")
				end)
				session:stop()
				instance:Destroy()
				assert(success, `{className} {tostring(responseFirst)} {tostring(problem)}`)
			end
		end
	end)

	it(
		"should distinguish later filesystem Source from earlier echoes while a deferred response is in flight",
		function()
			for _, responseFirst in { false, true } do
				for _, laterSource in { false, true } do
					local session, instance, control = propertySession("ModuleScript")
					session.__applyPatch = function(_, patch)
						for _, update in patch.updated do
							if update.changedProperties.Source then
								instance.Source = update.changedProperties.Source.String
							end
						end
					end
					local success, problem = pcall(function()
						instance.Source = "local value ="
						session:__queueChange(instance, "Source")
						local write = session:flushChanges()
						local result = writeResult({
							{
								id = "EDITED",
								kind = "sourceSerializationDeferred",
								property = "Source",
								message = "The draft remains unsaved",
							},
						})
						result.messageCursor = 1
						if responseFirst then
							control.settle[1](result)
							write:await()
						end
						session:__protectLocalProperties({
							added = {},
							removed = {},
							updated = {
								{
									id = "EDITED",
									changedProperties = { Source = { String = "return 'previous save'" } },
								},
							},
						}, 1)
						session:__protectLocalProperties({
							added = {},
							removed = {},
							updated = {
								{
									id = if laterSource then "EDITED" else "OTHER",
									changedProperties = { Source = { String = "return 'external edit'" } },
								},
							},
						}, 2)
						control.cursor = 2
						session:__releaseLocalProperties(control.cursor)
						if not responseFirst then
							control.settle[1](result)
							write:await()
						end
						expect(instance.Source).to.equal(
							if laterSource then "return 'external edit'" else "local value ="
						)
						expect(session:getSyncResults()[1].status).to.equal("Failed")
						expect(next(session.__localPropertyEdits)).to.equal(nil)
					end)
					session:stop()
					instance:Destroy()
					assert(success, problem)
				end
			end
		end
	)

	it("should retain filesystem Source authority until a conflicted write is fully acknowledged", function()
		for _, responseFirst in { false, true } do
			local session, instance, control = propertySession("ModuleScript")
			session.__applyPatch = function(_, patch)
				for _, update in patch.updated do
					if update.changedProperties.Source then
						instance.Source = update.changedProperties.Source.String
					end
				end
			end
			local success, problem = pcall(function()
				instance.Source = "return 'Studio'"
				session:__queueChange(instance, "Source")
				local write = session:flushChanges()
				local result = writeResult(nil, {
					removed_instances = {},
					added_instances = {},
					updated_instances = { { id = "EDITED" } },
				})
				result.messageCursor = 1
				if responseFirst then
					control.settle[1](result)
					write:await()
				end
				local filtered = session:__protectLocalProperties({
					added = {},
					removed = {},
					updated = { { id = "EDITED", changedProperties = { Source = { String = "return 'Filesystem'" } } } },
				})
				expect(#filtered.updated).to.equal(0)
				expect(instance.Source).to.equal("return 'Studio'")
				control.cursor = 1
				session:__releaseLocalProperties(control.cursor)
				if not responseFirst then
					control.settle[1](result)
					write:await()
				end
				expect(instance.Source).to.equal("return 'Filesystem'")
				expect(session:getSyncResults()[1].status).to.equal("Conflicted")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			session:stop()
			instance:Destroy()
			assert(success, problem)
		end
	end)

	it("should capture undelivered structure before an older acknowledgement releases its guard", function()
		for _, publicationFirst in { false, true } do
			forDeferredStructures(function(propertyName, session, _, control, field)
				local firstEntry = control.edit("DESTINATION")
				local first = session:flushChanges()
				if publicationFirst then
					control.publish("DESTINATION", 1)
				end
				control.assign("PROBE")
				control.cursor = 1
				control.answer(1, 1)
				first:await()
				if not publicationFirst then
					control.publish("DESTINATION", 1)
				end
				control.expectValue("PROBE")
				local newer = session.__localPropertyEdits.EDITED[propertyName]
				expect(newer ~= nil and newer ~= firstEntry).to.equal(true)
				expect(newer.cursor).to.equal(nil)
				control.deliver()
				control.deliver()
				expect(session.__localPropertyEdits.EDITED[propertyName]).to.equal(newer)
				local second = session:flushChanges()
				expect(control.requests[2].updated[1][field]).to.equal("PROBE")
				control.publish("PROBE", 2)
				control.cursor = 2
				control.answer(2, 2)
				second:await()
				control.expectValue("PROBE")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
		end
	end)

	it("should capture undelivered structure without a guard before independent disk validation", function()
		forDeferredStructures(function(propertyName, session, _, control, field)
			control.assign("DESTINATION")
			control.publish("DISK", 1)
			control.expectValue("DESTINATION")
			local entry = session.__localPropertyEdits.EDITED[propertyName]
			expect(entry ~= nil and entry.cursor == nil).to.equal(true)
			local write = session:flushChanges()
			expect(control.requests[1].updated[1][field]).to.equal("DESTINATION")
			control.cursor = 1
			control.answer(1, 1, {
				removed_instances = {},
				added_instances = {},
				updated_instances = {
					{
						id = "EDITED",
						changed_properties = {},
						[if propertyName == "Parent" then "changedParent" else "changed_name"] = "DISK",
					},
				},
			})
			write:await()
			control.expectValue("DISK")
			control.deliver()
			control.deliver()
			expect(next(session.__localPropertyEdits)).to.equal(nil)
			expect(session:getSyncResults()[1].status).to.equal("Conflicted")
		end)
	end)

	it("should recapture undelivered structure after a yielding precommit boundary", function()
		forDeferredStructures(function(propertyName, session, _, control)
			local resume
			local gate = Promise.new(function(resolve)
				resume = resolve
			end)
			local entered = false
			session:hookPrecommit(function()
				entered = true
				local resumed = gate:await()
				assert(resumed, "The withheld structural precommit did not resume.")
			end)
			local application = Promise.new(function(resolve, reject)
				local result = control.publish("PROBE", 1)
				if result ~= nil then
					result:andThen(resolve):catch(reject)
				else
					resolve()
				end
			end)
			expect(entered).to.equal(true)
			control.assign("DESTINATION")
			resume()
			local applied, err = application:await()
			assert(applied, tostring(err))
			control.expectValue("DESTINATION")
			expect(session.__localPropertyEdits.EDITED[propertyName] ~= nil).to.equal(true)
			control.deliver()
			local write = session:flushChanges()
			control.publish("DESTINATION", 2)
			control.cursor = 2
			control.answer(1, 2)
			write:await()
			control.expectValue("DESTINATION")
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
	end)

	it("should retain late undelivered structure if the engine boundary overtakes filtering", function()
		forDeferredStructures(function(_propertyName, session, _, control)
			local apply = session.__reconciler.applyPatch
			session.__reconciler.applyPatch = function(self, patch)
				control.assign("DESTINATION")
				return apply(self, patch)
			end
			local application = control.publish("PROBE", 1)
			expect(application ~= nil).to.equal(true)
			local applied = application:await()
			expect(applied).to.equal(false)
			control.expectValue("DESTINATION")
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(#control.requests).to.equal(0)
		end)
	end)

	it("should discard captured structure when its authoritative identity is removed", function()
		forDeferredStructures(function(_propertyName, session, instance, control)
			control.assign("DESTINATION")
			session:__applyPatch({ added = {}, removed = { "EDITED" }, updated = {} }, 1)
			expect(session.__instanceMap.fromIds.EDITED).to.equal(nil)
			expect(session.__localPropertyEdits.EDITED).to.equal(nil)
			local replacement = Instance.new("Actor")
			replacement.Parent = session.__instanceMap.fromIds.PROBE
			session.__instanceMap:insert("EDITED", replacement)
			session:__releaseLocalProperties(2)
			expect(replacement.Name).to.equal("Actor")
			expect(replacement.Parent).to.equal(session.__instanceMap.fromIds.PROBE)
			expect(instance.Parent).to.equal(nil)
		end)
	end)

	it("should not replay captured structure onto a class replacement", function()
		forDeferredStructures(function(_propertyName, session, instance, control)
			control.assign("DESTINATION")
			session:__applyPatch({
				added = {},
				removed = {},
				updated = {
					{
						id = "EDITED",
						changedClassName = "Folder",
						changedName = "Canonical",
						changedParent = "DISK",
						changedProperties = {},
					},
				},
			}, 1)
			local replacement = session.__instanceMap.fromIds.EDITED
			expect(replacement ~= instance).to.equal(true)
			expect(replacement.ClassName).to.equal("Folder")
			expect(replacement.Name).to.equal("Canonical")
			expect(replacement.Parent).to.equal(session.__instanceMap.fromIds.DISK)
			control.deliver()
			session:__releaseLocalProperties(2)
			expect(session.__localPropertyEdits.EDITED).to.equal(nil)
			expect(replacement.Name).to.equal("Canonical")
		end)
	end)

	it("should abandon reconciliation if deferred structural capture stops its session", function()
		forDeferredStructures(function(_propertyName, session, _, control)
			session.__instanceMap.onInstanceChanged = function()
				session:stop()
			end
			control.assign("DESTINATION")
			local application = control.publish("PROBE", 1)
			expect(application ~= nil).to.equal(true)
			local applied = application:await()
			expect(applied).to.equal(false)
			control.expectValue("DESTINATION")
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(control.cursor).to.equal(0)
			expect(#control.requests).to.equal(0)
		end)
	end)

	for _, propertyName in { "Parent", "Name" } do
		it(
			`should guard rapid {propertyName} generations through older acknowledgements and duplicate publications`,
			function()
				for _, order in { { false, false }, { false, true }, { true, false }, { true, true } } do
					local responseFirst, takeSecond = order[1], order[2]
					local session, instance, control, field = structuralSession(propertyName)
					local success, problem = pcall(function()
						local firstEntry = control.edit("DESTINATION")
						local first = session:flushChanges()
						local secondEntry = control.edit("PROBE")
						local second = if takeSecond then session:flushChanges() else nil
						if responseFirst then
							control.answer(1, 1)
							first:await()
						end
						for _ = 1, 2 do
							control.publish("DESTINATION", 1)
							control.expectValue("PROBE")
						end
						expect(firstEntry ~= nil and secondEntry ~= nil and firstEntry ~= secondEntry).to.equal(true)
						control.cursor = 1
						session:__releaseLocalProperties(1)
						if not responseFirst then
							control.answer(1, 1)
							first:await()
						end
						expect(session.__localPropertyEdits.EDITED[propertyName]).to.equal(secondEntry)
						expect(secondEntry.cursor).to.equal(nil)
						if second == nil then
							second = session:flushChanges()
						end
						expect(control.requests[2].updated[1][field]).to.equal("PROBE")
						expect(control.requests[2].updated[1].changedProperties[propertyName]).to.equal(nil)
						control.answer(2, 2)
						second:await()
						session:__finishLocalProperties(
							{ EDITED = { [propertyName] = firstEntry } },
							{ messageCursor = 1 }
						)
						expect(session.__localPropertyEdits.EDITED[propertyName]).to.equal(secondEntry)
						expect(secondEntry.cursor).to.equal(2)
						control.publish("PROBE", 2)
						control.cursor = 2
						session:__releaseLocalProperties(2)
						control.expectValue("PROBE")
						expect(next(session.__localPropertyEdits)).to.equal(nil)
						expect(instance.Parent ~= nil).to.equal(true)
					end)
					control.cleanup()
					assert(success, problem)
				end
			end
		)

		it(`should retain pending {propertyName} across same-identity ancestor moves and renames`, function()
			local session, _, control = structuralSession(propertyName)
			local success, problem = pcall(function()
				local entry = control.edit("DESTINATION")
				expect(entry ~= nil).to.equal(true)
				local write = session:flushChanges()
				session:__applyPatch({
					added = {},
					removed = {},
					updated = {
						{ id = "PROBE", changedName = "MovedProbe", changedParent = "DISK", changedProperties = {} },
					},
				}, 1)
				control.publish("PROBE", 2)
				control.expectValue("DESTINATION")
				expect(session.__localPropertyEdits.EDITED[propertyName]).to.equal(entry)
				control.answer(1, 3)
				write:await()
				control.publish("DESTINATION", 3)
				control.cursor = 3
				session:__releaseLocalProperties(3)
				control.expectValue("DESTINATION")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			control.cleanup()
			assert(success, problem)
		end)

		it(
			`should release the exact independent disk {propertyName} after validation in either completion order`,
			function()
				for _, responseFirst in { false, true } do
					local session, _, control = structuralSession(propertyName)
					local success, problem = pcall(function()
						local entry = control.edit("DESTINATION")
						expect(entry ~= nil).to.equal(true)
						local write = session:flushChanges()
						local conflict = {
							removed_instances = {},
							added_instances = {},
							updated_instances = {
								{
									id = "EDITED",
									changed_properties = {},
									[if propertyName == "Parent" then "changedParent" else "changed_name"] = "DISK",
								},
							},
						}
						if responseFirst then
							control.answer(1, 2, conflict)
							write:await()
						end
						control.publish("DESTINATION", 1)
						control.publish("DISK", 2)
						control.expectValue("DESTINATION")
						expect(session.__localPropertyEdits.EDITED[propertyName]).to.equal(entry)
						control.cursor = 2
						session:__releaseLocalProperties(2)
						if not responseFirst then
							control.answer(1, 2, conflict)
							write:await()
						end
						control.expectValue("DISK")
						expect(next(session.__localPropertyEdits)).to.equal(nil)
					end)
					control.cleanup()
					assert(success, problem)
				end
			end
		)

		it(
			`should retain buffered {propertyName} authority when a newer generation is refused without republication`,
			function()
				local session, _, control = structuralSession(propertyName)
				local success, problem = pcall(function()
					control.edit("DESTINATION")
					local first = session:flushChanges()
					control.publish("DISK", 1)
					local newer = control.edit("PROBE")
					local second = session:flushChanges()
					control.cursor = 1
					control.answer(1, 1)
					first:await()
					expect(session.__localPropertyEdits.EDITED[propertyName]).to.equal(newer)
					control.expectValue("PROBE")
					control.answer(2, 1, {
						removed_instances = {},
						added_instances = {},
						updated_instances = { { id = "EDITED", changed_properties = {} } },
					})
					second:await()
					control.expectValue("DISK")
					expect(next(session.__localPropertyEdits)).to.equal(nil)
				end)
				control.cleanup()
				assert(success, problem)
			end
		)

		it(`should keep newer {propertyName} publications ahead of delayed buffered replay`, function()
			local session, _, control, field = structuralSession(propertyName)
			local success, problem = pcall(function()
				control.edit("DESTINATION")
				local write = session:flushChanges()
				control.publish("PROBE", 1)
				control.answer(1, 1)
				write:await()
				local resume
				session.__pendingSourcePatch = Promise.new(function(resolve)
					resume = resolve
				end)
				control.cursor = 1
				local replay = session:__releaseLocalProperties(1)
				local publication = control.publish("DISK", 2)
				local applied = {}
				session:hookPrecommit(function(patch)
					for _, update in patch.updated do
						if update.id == "EDITED" and update[field] ~= nil then
							table.insert(applied, update[field])
						end
					end
				end)
				resume()
				replay:await()
				publication:await()
				control.expectValue("DISK")
				expect(#applied).to.equal(1)
				expect(applied[1]).to.equal("DISK")
			end)
			control.cleanup()
			assert(success, problem)
		end)

		it(`should remember queued WebSocket {propertyName} receipts before an earlier replay completes`, function()
			local session, _, control, field = structuralSession(propertyName)
			local success, problem = pcall(function()
				local messages
				session.__apiContext.connect = function()
					return Promise.resolve({ projectName = "StructuralReceiptTest" })
				end
				session.__apiContext.connectWebSocket = function(_, handlers)
					messages = handlers.messages
					return Promise.resolve()
				end
				session.__apiContext.setMessageCursor = function(_, cursor)
					control.cursor = cursor
				end
				session.__initialSync = function()
					return Promise.resolve()
				end
				session:start()
				expect(messages ~= nil).to.equal(true)
				control.edit("DESTINATION")
				local write = session:flushChanges()
				control.publish("PROBE", 1)
				control.answer(1, 1)
				write:await()
				local resume
				session.__pendingSourcePatch = Promise.new(function(resolve)
					resume = resolve
				end)
				control.cursor = 1
				local replay = session:__releaseLocalProperties(1)
				session.__lastPublication = replay
				messages({
					messageCursor = 2,
					messages = {
						{
							added = {},
							removed = {},
							updated = { { id = "EDITED", [field] = "DISK", changedProperties = {} } },
						},
					},
				})
				local applied = {}
				session:hookPrecommit(function(patch)
					for _, update in patch.updated do
						if update.id == "EDITED" and update[field] ~= nil then
							table.insert(applied, update[field])
						end
					end
				end)
				resume()
				session.__lastPublication:await()
				control.expectValue("DISK")
				expect(#applied).to.equal(1)
				expect(applied[1]).to.equal("DISK")
				expect(control.cursor).to.equal(2)
				expect(session:getStatus()).to.equal(ServeSession.Status.Connected)
			end)
			control.cleanup()
			assert(success, problem)
		end)

		it(`should stop unknown {propertyName} transport without replaying an older buffered value`, function()
			local session, _, control = structuralSession(propertyName)
			local success, problem = pcall(function()
				local fail
				session.__apiContext.write = function()
					return Promise.new(function(_, reject)
						fail = reject
					end)
				end
				control.edit("DESTINATION")
				local write = session:flushChanges()
				control.publish("PROBE", 1)
				control.cursor = 1
				fail("The write outcome is unknown.")
				write:await()
				control.expectValue("DESTINATION")
				expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
				expect(session.__localPropertyEdits).to.equal(nil)
			end)
			control.cleanup()
			assert(success, problem)
		end)

		it(`should retain deferred {propertyName} intent through initial structural catch-up`, function()
			local session, instance, control = structuralSession(propertyName)
			local success, problem = pcall(function()
				session.__deferredChanges = {}
				instance[propertyName] = if propertyName == "Parent"
					then session.__instanceMap.fromIds.DESTINATION
					else "DESTINATION"
				session:__onInstanceChanged(instance, propertyName)
				expect(next(session.__localPropertyEdits)).to.equal(nil)
				session:__releaseDeferredChanges()
				local entry = session.__localPropertyEdits.EDITED[propertyName]
				expect(entry ~= nil).to.equal(true)
				control.publish("PROBE", 0)
				control.expectValue("DESTINATION")
				local write = session:flushChanges()
				control.publish("DESTINATION", 1)
				control.cursor = 1
				control.answer(1, 1)
				write:await()
				control.expectValue("DESTINATION")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			control.cleanup()
			assert(success, problem)
		end)

		it(`should allow authoritative escape before ancestor removal without pinning {propertyName}`, function()
			for _, actualInstanceRemoval in { false, true } do
				local session, instance, control = structuralSession(propertyName)
				local success, problem = pcall(function()
					control.edit("DESTINATION")
					local write = session:flushChanges()
					local probe = session.__instanceMap.fromIds.PROBE
					session:__applyPatch({
						added = {},
						removed = { if actualInstanceRemoval then probe else "PROBE" },
						updated = {
							{
								id = "EDITED",
								changedName = "ServerSurvivor",
								changedParent = "DISK",
								changedProperties = {},
							},
						},
					}, 1)
					expect(session.__instanceMap.fromIds.EDITED).to.equal(instance)
					expect(instance.Parent).to.equal(session.__instanceMap.fromIds.DISK)
					expect(instance.Name).to.equal("ServerSurvivor")
					expect(next(session.__localPropertyEdits)).to.equal(nil)
					control.cursor = 1
					control.answer(1, 1)
					write:await()
				end)
				control.cleanup()
				assert(success, problem)
			end
		end)

		it(
			`should release the newest buffered {propertyName} cursor despite reordered duplicate announcements`,
			function()
				local session, _, control = structuralSession(propertyName)
				local success, problem = pcall(function()
					control.edit("DESTINATION")
					local write = session:flushChanges()
					control.publish("DISK", 2)
					control.publish("PROBE", 1)
					control.publish("PROBE", 1)
					control.expectValue("DESTINATION")
					control.cursor = 2
					control.answer(1, 2)
					write:await()
					control.expectValue("DISK")
					expect(next(session.__localPropertyEdits)).to.equal(nil)
					control.publish("PROBE", 3)
					control.expectValue("PROBE")
				end)
				control.cleanup()
				assert(success, problem)
			end
		)

		it(`should discard {propertyName} guards on same-ID class replacement`, function()
			local session, instance, control = structuralSession(propertyName)
			local success, problem = pcall(function()
				control.edit("DESTINATION")
				local write = session:flushChanges()
				control.publish("PROBE", 1)
				session:__applyPatch({
					added = {},
					removed = {},
					updated = {
						{
							id = "EDITED",
							changedClassName = "Folder",
							changedName = "ClassReplacement",
							changedProperties = {},
						},
					},
				}, 2)
				local replacement = session.__instanceMap.fromIds.EDITED
				expect(replacement == instance).to.equal(false)
				expect(replacement.Name).to.equal("ClassReplacement")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
				control.cursor = 2
				control.answer(1, 2)
				write:await()
				expect(replacement.Name).to.equal("ClassReplacement")
			end)
			control.cleanup()
			assert(success, problem)
		end)

		it(`should release explicit failed {propertyName} responses through exact server restoration`, function()
			local session, _, control = structuralSession(propertyName)
			local success, problem = pcall(function()
				control.edit("DESTINATION")
				local write = session:flushChanges()
				control.publish("DISK", 1)
				control.expectValue("DESTINATION")
				local result =
					writeResult({ { id = "EDITED", property = propertyName, message = "The field is excluded." } })
				result.messageCursor = 1
				control.cursor = 1
				control.settle[1](result)
				write:await()
				control.expectValue("DISK")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
				expect(session:getSyncResults()[1].status).to.equal("Failed")
				expect(session:getStatus() == ServeSession.Status.Disconnected).to.equal(false)
			end)
			control.cleanup()
			assert(success, problem)
		end)

		it(`should clear omitted and validated no-op {propertyName} generations without publications`, function()
			local session, instance, control, field = structuralSession(propertyName)
			local success, problem = pcall(function()
				local omitted = control.edit("DESTINATION")
				expect(omitted ~= nil).to.equal(true)
				local taken = session:__takeLocalPropertyEdits({ [instance] = { [propertyName] = true } })
				expect(taken.EDITED[propertyName]).to.equal(omitted)
				session:__finishOmittedLocalProperties({ added = {}, removed = {}, updated = {} }, taken)
				expect(next(session.__localPropertyEdits)).to.equal(nil)
				local newer = control.edit("PROBE")
				session:__finishOmittedLocalProperties(nil, taken)
				expect(session.__localPropertyEdits.EDITED[propertyName]).to.equal(newer)
				local write = session:flushChanges()
				expect(control.requests[1].updated[1][field]).to.equal("PROBE")
				control.answer(1, 0)
				write:await()
				control.expectValue("PROBE")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			control.cleanup()
			assert(success, problem)
		end)

		it(`should discard pending {propertyName} on removal and never replay onto a replacement identity`, function()
			for _, replacement in { false, true } do
				local session, instance, control = structuralSession(propertyName)
				local success, problem = pcall(function()
					local entry = control.edit("DESTINATION")
					expect(entry ~= nil).to.equal(true)
					local write = session:flushChanges()
					control.publish("PROBE", 1)
					session:__applyPatch({
						added = if replacement
							then {
								EDITED = {
									Id = "EDITED",
									Parent = "DISK",
									Name = "ServerReplacement",
									ClassName = "Folder",
									Properties = {},
									Children = {},
								},
							}
							else {},
						removed = { "EDITED" },
						updated = {},
					}, 2)
					expect(next(session.__localPropertyEdits)).to.equal(nil)
					control.cursor = 2
					control.answer(1, 2)
					write:await()
					local current = session.__instanceMap.fromIds.EDITED
					if replacement then
						expect(current == instance).to.equal(false)
						expect(current.Name).to.equal("ServerReplacement")
						expect(current.Parent).to.equal(session.__instanceMap.fromIds.DISK)
					else
						expect(current).to.equal(nil)
					end
				end)
				control.cleanup()
				assert(success, problem)
			end
		end)
	end

	it("should restore confirmed ancestry before an authoritative move reverses a pending optimistic edge", function()
		for _, responseFirst in { false, true } do
			local session, actor, control = structuralSession("Parent")
			local success, problem = pcall(function()
				control.edit("DESTINATION")
				local write = session:flushChanges()
				local conflict = {
					removed_instances = {},
					added_instances = {},
					updated_instances = { { id = "EDITED", changedParent = "PROBE", changed_properties = {} } },
				}
				if responseFirst then
					control.answer(1, 2, conflict)
					write:await()
				end
				local destination = session.__instanceMap.fromIds.DESTINATION
				local apply = session.__reconciler.applyPatch
				session.__reconciler.applyPatch = function(self, patch)
					local unapplied = apply(self, patch)
					expect(#unapplied.updated).to.equal(0)
					return unapplied
				end
				session:__applyPatch({
					added = {},
					removed = {},
					updated = {
						{ id = "DESTINATION", changedParent = "EDITED", changedProperties = {} },
					},
				}, 1)
				expect(destination.Parent).to.equal(actor)
				control.expectValue("PROBE")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
				control.publish("PROBE", 2)
				control.cursor = 2
				if not responseFirst then
					control.answer(1, 2, conflict)
					write:await()
				end
				expect(destination.Parent).to.equal(actor)
				control.expectValue("PROBE")
				expect(session:getStatus() == ServeSession.Status.Disconnected).to.equal(false)
			end)
			control.cleanup()
			assert(success, problem)
		end
	end)

	it("should use the packet's exact Parent correction before reversing a buffered optimistic edge", function()
		local session, actor, control = structuralSession("Parent")
		local success, problem = pcall(function()
			control.edit("DESTINATION")
			local write = session:flushChanges()
			control.publish("DESTINATION", 1)
			local destination = session.__instanceMap.fromIds.DESTINATION
			local apply = session.__reconciler.applyPatch
			session.__reconciler.applyPatch = function(self, patch)
				local unapplied = apply(self, patch)
				expect(#unapplied.updated).to.equal(0)
				return unapplied
			end
			session:__applyPatch({
				added = {},
				removed = {},
				updated = {
					{ id = "DESTINATION", changedParent = "EDITED", changedProperties = {} },
					{ id = "EDITED", changedParent = "DISK", changedProperties = {} },
				},
			}, 2)
			control.expectValue("DISK")
			expect(destination.Parent).to.equal(actor)
			expect(next(session.__localPropertyEdits)).to.equal(nil)
			control.cursor = 2
			control.answer(1, 2)
			write:await()
			control.expectValue("DISK")
			expect(destination.Parent).to.equal(actor)
		end)
		control.cleanup()
		assert(success, problem)
	end)

	it("should stop an unprovable structural cycle without losing local ancestry or consuming its cursor", function()
		local session, actor, control = structuralSession("Parent")
		local success, problem = pcall(function()
			control.edit("DESTINATION")
			control.publish("DESTINATION", 1)
			local destination = session.__instanceMap.fromIds.DESTINATION
			local publication = session:__applyPatch({
				added = {},
				removed = {},
				updated = {
					{ id = "DESTINATION", changedParent = "EDITED", changedProperties = {} },
				},
			}, 2)
			local applied = publication:await()
			expect(applied).to.equal(false)
			expect(actor.Parent).to.equal(destination)
			expect(destination.Parent == actor).to.equal(false)
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(session.__localPropertyEdits).to.equal(nil)
			expect(control.cursor).to.equal(0)
		end)
		control.cleanup()
		assert(success, problem)
	end)

	it("should stop a reversing publication before it rewrites an unencoded Parent intention", function()
		local session, actor, control = structuralSession("Parent")
		local success, problem = pcall(function()
			control.edit("DESTINATION")
			local destination = session.__instanceMap.fromIds.DESTINATION
			local publication = session:__applyPatch({
				added = {},
				removed = {},
				updated = {
					{ id = "DESTINATION", changedParent = "EDITED", changedProperties = {} },
				},
			}, 1)
			expect(publication ~= nil).to.equal(true)
			expect(publication:await()).to.equal(false)
			expect(actor.Parent).to.equal(destination)
			expect(destination.Parent == actor).to.equal(false)
			expect(session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(session.__localPropertyEdits).to.equal(nil)
			expect(#control.requests).to.equal(0)
			expect(control.cursor).to.equal(0)
		end)
		control.cleanup()
		assert(success, problem)
	end)

	it("should prune structural guards after identity replacement during a nonstructural reconciliation", function()
		for _, propertyName in { "Parent", "Name" } do
			local session, instance, control = structuralSession(propertyName)
			local success, problem = pcall(function()
				control.edit("DESTINATION")
				local write = session:flushChanges()
				local replacement = Instance.new("Actor")
				replacement.Name = "FallbackReplacement"
				replacement.Parent = session.__instanceMap.fromIds.DISK
				session.__reconciler.applyPatch = function()
					session.__instanceMap:insert("EDITED", replacement)
					return { added = {}, removed = {}, updated = {} }
				end
				session:__applyPatch({
					added = {},
					removed = {},
					updated = { { id = "EDITED", changedProperties = { Archivable = { Bool = true } } } },
				}, 1)
				expect(session.__instanceMap.fromIds.EDITED == instance).to.equal(false)
				expect(next(session.__localPropertyEdits)).to.equal(nil)
				control.answer(1, 0)
				write:await()
				expect(replacement.Name).to.equal("FallbackReplacement")
				expect(replacement.Parent).to.equal(session.__instanceMap.fromIds.DISK)
			end)
			control.cleanup()
			assert(success, problem)
		end
	end)

	it("should transfer a nil Parent intent to deletion without replaying its buffered old parent", function()
		local session, instance, control = structuralSession("Parent")
		local success, problem = pcall(function()
			control.edit("DESTINATION")
			control.publish("DISK", 1)
			instance.Parent = nil
			session:__queueChange(instance, "Parent")
			local write = session:flushChanges()
			expect(control.requests[1].removed[1]).to.equal("EDITED")
			expect(instance.Parent).to.equal(nil)
			expect(next(session.__localPropertyEdits)).to.equal(nil)
			local result = writeResult()
			result.accepted.removed_instances = { "EDITED" }
			control.settle[1](result)
			write:await()
			expect(instance.Parent).to.equal(nil)
			expect(session.__instanceMap.fromIds.EDITED).to.equal(nil)
			expect(session.__changeBatcher:isEmpty()).to.equal(true)
		end)
		control.cleanup()
		assert(success, problem)
	end)

	it("should clear Parent guards when the real encoder restores an unmanaged destination", function()
		local session, instance, control = structuralSession("Parent")
		local unmanaged = Instance.new("Folder")
		local success, problem = pcall(function()
			instance.Parent = unmanaged
			session:__queueChange(instance, "Parent")
			expect(session.__localPropertyEdits.EDITED.Parent ~= nil).to.equal(true)
			expect(session:flushChanges()).to.equal(nil)
			control.expectValue("PROBE")
			expect(next(session.__localPropertyEdits)).to.equal(nil)
			expect(#control.requests).to.equal(0)
		end)
		control.cleanup()
		unmanaged:Destroy()
		assert(success, problem)
	end)

	it("should preserve newer local properties across delayed server echoes at every write stage", function()
		for _, stage in { "pending", "queued", "inflight", "answered" } do
			local session, part, control = propertySession("Part")
			local function edit(width)
				part.Size = Vector3.new(width, 2, 2)
				session:__queueChange(part, "Size")
			end
			local function echo(width)
				session:__applyPatch({
					added = {},
					removed = {},
					updated = {
						{ id = "EDITED", changedProperties = { Size = { Vector3 = { width, 2, 2 } } } },
					},
				})
			end
			local function answer(index, revision)
				local result = writeResult()
				result.messageCursor = revision
				control.settle[index](result)
			end
			local success, problem = pcall(function()
				edit(4)
				local first = session:flushChanges()
				edit(5)
				local second = if stage ~= "pending" then session:flushChanges() else nil
				if stage == "inflight" or stage == "answered" then
					answer(1, 1)
					first:await()
				end
				if stage == "answered" then
					answer(2, 2)
					second:await()
				end
				echo(4)
				expect(part.Size).to.equal(Vector3.new(5, 2, 2))
				control.cursor = 1
				session:__releaseLocalProperties(control.cursor)
				if stage == "pending" or stage == "queued" then
					answer(1, 1)
					first:await()
				end
				if second == nil then
					second = session:flushChanges()
				end
				expect(control.requests[2].updated[1].changedProperties.Size.Vector3[1]).to.equal(5)
				if stage ~= "answered" then
					answer(2, 2)
					second:await()
				end
				echo(5)
				control.cursor = 2
				session:__releaseLocalProperties(control.cursor)
				expect(part.Size).to.equal(Vector3.new(5, 2, 2))
				echo(6)
				expect(part.Size).to.equal(Vector3.new(6, 2, 2))
			end)
			session:stop()
			part:Destroy()
			assert(success, `{stage} {tostring(problem)}`)
		end
	end)

	it("should retain buffered authoritative values across unrelated structural patches", function()
		local session, part, control = propertySession("Part")
		local unrelated = Instance.new("Folder")
		session.__instanceMap:insert("UNRELATED", unrelated)
		local success, problem = pcall(function()
			part.Size = Vector3.new(5, 2, 2)
			session:__queueChange(part, "Size")
			local write = session:flushChanges()
			for _, width in { 5, 6 } do
				session:__applyPatch({
					added = {},
					removed = {},
					updated = { { id = "EDITED", changedProperties = { Size = { Vector3 = { width, 2, 2 } } } } },
				})
			end
			session:__applyPatch({
				added = {},
				removed = {},
				updated = { { id = "UNRELATED", changedName = "ServerName", changedProperties = {} } },
			})
			expect(unrelated.Name).to.equal("ServerName")
			control.cursor = 3
			local result = writeResult()
			result.messageCursor = 1
			control.settle[1](result)
			write:await()
			expect(part.Size).to.equal(Vector3.new(6, 2, 2))
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
		session:stop()
		part:Destroy()
		unrelated:Destroy()
		assert(success, problem)
	end)

	it("should keep pending local properties protected when another instance is renamed", function()
		local session, part = propertySession("Part")
		local unrelated = Instance.new("Folder")
		session.__instanceMap:insert("UNRELATED", unrelated)
		local success, problem = pcall(function()
			part.Size = Vector3.new(5, 2, 2)
			session:__queueChange(part, "Size")
			session:__applyPatch({
				added = {},
				removed = {},
				updated = {
					{ id = "UNRELATED", changedName = "ServerName", changedProperties = {} },
					{ id = "EDITED", changedProperties = { Size = { Vector3 = { 4, 2, 2 } } } },
				},
			})
			expect(part.Size).to.equal(Vector3.new(5, 2, 2))
		end)
		session:stop()
		part:Destroy()
		unrelated:Destroy()
		assert(success, problem)
	end)

	for _, action in { "renamed", "reclassified", "removed" } do
		it(`should preserve buffered properties on children retained when an ancestor is {action}`, function()
			local session, part = propertySession("Part")
			local root = Instance.new("Folder")
			local parent = Instance.new("Folder")
			parent.Parent = root
			part.Parent = parent
			session.__instanceMap:insert("ROOT", root)
			session.__instanceMap:insert("PARENT", parent)
			session.__instanceMap.knownParents[part] = parent
			local success, problem = pcall(function()
				part.Size = Vector3.new(5, 2, 2)
				session:__queueChange(part, "Size")
				session:__applyPatch({
					added = {},
					removed = {},
					updated = { { id = "EDITED", changedProperties = { Size = { Vector3 = { 6, 2, 2 } } } } },
				})
				local update = { id = "PARENT", changedProperties = {} }
				if action == "renamed" then
					update.changedName = "ServerName"
				elseif action == "reclassified" then
					update.changedClassName = "Model"
				else
					update = { id = "EDITED", changedParent = "ROOT", changedProperties = {} }
				end
				session:__applyPatch({
					added = {},
					removed = if action == "removed" then { "PARENT" } else {},
					updated = { update },
				})
				if action == "renamed" then
					expect(parent.Name).to.equal("ServerName")
				elseif action == "reclassified" then
					expect(part.Parent.ClassName).to.equal("Model")
				else
					expect(part.Parent).to.equal(root)
				end
				expect(session.__instanceMap.fromIds.EDITED).to.equal(part)
				expect(part.Size).to.equal(Vector3.new(6, 2, 2))
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			session:stop()
			root:Destroy()
			parent:Destroy()
			assert(success, problem)
		end)
	end

	it("should not replay old buffered values onto a recreated authoritative subtree", function()
		local session, part = propertySession("Part")
		local root = Instance.new("Folder")
		local parent = Instance.new("Folder")
		parent.Parent = root
		part.Parent = parent
		session.__instanceMap:insert("ROOT", root)
		session.__instanceMap:insert("PARENT", parent)
		session.__instanceMap.knownParents[part] = parent
		local success, problem = pcall(function()
			part.Size = Vector3.new(5, 2, 2)
			session:__queueChange(part, "Size")
			session:__applyPatch({
				added = {},
				removed = {},
				updated = { { id = "EDITED", changedProperties = { Size = { Vector3 = { 6, 2, 2 } } } } },
			})
			session:__applyPatch({
				added = {
					PARENT = {
						Id = "PARENT",
						Parent = "ROOT",
						Name = "Replacement",
						ClassName = "Model",
						Properties = {},
						Children = { "EDITED" },
					},
					EDITED = {
						Id = "EDITED",
						Parent = "PARENT",
						Name = "ReplacementPart",
						ClassName = "Part",
						Properties = { Size = { Vector3 = { 8, 2, 2 } } },
						Children = {},
					},
				},
				removed = { "PARENT" },
				updated = {},
			})
			local replacement = session.__instanceMap.fromIds.EDITED
			expect(replacement == part).to.equal(false)
			expect(replacement.Size).to.equal(Vector3.new(8, 2, 2))
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
		session:stop()
		root:Destroy()
		parent:Destroy()
		assert(success, problem)
	end)

	it("should protect ordinary model-backed string values across delayed echoes", function()
		local session, value, control = propertySession("StringValue")
		local success, problem = pcall(function()
			value.Value = "first"
			session:__queueChange(value, "Value")
			local first = session:flushChanges()
			value.Value = "second"
			session:__queueChange(value, "Value")
			session:__applyPatch({
				added = {},
				removed = {},
				updated = { { id = "EDITED", changedProperties = { Value = { String = "first" } } } },
			})
			expect(value.Value).to.equal("second")
			local result = writeResult()
			result.messageCursor = 1
			control.settle[1](result)
			first:await()
			local second = session:flushChanges()
			expect(control.requests[2].updated[1].changedProperties.Value.String).to.equal("second")
			result.messageCursor = 2
			control.settle[2](result)
			second:await()
		end)
		session:stop()
		value:Destroy()
		assert(success, problem)
	end)

	it("should not retain local generations for DataModel changes the batcher ignores", function()
		local session, unused, control = propertySession("Folder")
		session.__instanceMap:insert("ROOT", game)
		local success, problem = pcall(function()
			session:__queueChange(game, "Attributes")
			expect(session:flushChanges()).to.equal(nil)
			expect(#control.requests).to.equal(0)
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
		session:stop()
		unused:Destroy()
		assert(success, problem)
	end)

	it("should apply a refused property's restoration regardless of response ordering", function()
		for _, responseFirst in { false, true } do
			local session, part, control = propertySession("Part")
			local success, problem = pcall(function()
				part.Size = Vector3.new(5, 2, 2)
				session:__queueChange(part, "Size")
				local write = session:flushChanges()
				local result = writeResult({ { message = "Property is excluded" } })
				result.messageCursor = 1
				if responseFirst then
					control.settle[1](result)
					write:await()
				end
				session:__applyPatch({
					added = {},
					removed = {},
					updated = {
						{ id = "EDITED", changedProperties = { Size = { Vector3 = { 2, 2, 2 } } } },
					},
				})
				expect(part.Size).to.equal(Vector3.new(5, 2, 2))
				control.cursor = 1
				session:__releaseLocalProperties(control.cursor)
				if not responseFirst then
					control.settle[1](result)
					write:await()
				end
				expect(part.Size).to.equal(Vector3.new(2, 2, 2))
				expect(session:getSyncResults()[1].status).to.equal("Failed")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			session:stop()
			part:Destroy()
			assert(success, problem)
		end
	end)

	it("should never suppress structural authority", function()
		local session, part = propertySession("Part")
		local success, problem = pcall(function()
			part.Size = Vector3.new(5, 2, 2)
			session:__queueChange(part, "Size")
			session:__applyPatch({
				added = {},
				removed = {},
				updated = {
					{
						id = "EDITED",
						changedName = "ServerName",
						changedProperties = { Size = { Vector3 = { 2, 2, 2 } } },
					},
				},
			})
			expect(part.Size).to.equal(Vector3.new(2, 2, 2))
			expect(part.Name).to.equal("ServerName")
			expect(next(session.__localPropertyEdits)).to.equal(nil)
		end)
		session:stop()
		part:Destroy()
		assert(success, problem)
	end)

	it("should preserve filesystem payload authority regardless of response ordering", function()
		for _, responseFirst in { false, true } do
			local session, value, control = propertySession("StringValue")
			local success, problem = pcall(function()
				value.Value = "Studio text"
				session:__queueChange(value, "Value")
				local write = session:flushChanges()
				local result = writeResult(nil, {
					removed_instances = {},
					added_instances = {},
					updated_instances = { { id = "EDITED" } },
				})
				result.messageCursor = 1
				if responseFirst then
					control.settle[1](result)
					write:await()
				end
				session:__applyPatch({
					added = {},
					removed = {},
					updated = { { id = "EDITED", changedProperties = { Value = { String = "Filesystem text" } } } },
				})
				control.cursor = 1
				session:__releaseLocalProperties(control.cursor)
				if not responseFirst then
					control.settle[1](result)
					write:await()
				end
				expect(value.Value).to.equal("Filesystem text")
				expect(session:getSyncResults()[1].status).to.equal("Conflicted")
				expect(next(session.__localPropertyEdits)).to.equal(nil)
			end)
			session:stop()
			value:Destroy()
			assert(success, problem)
		end
	end)

	it("should discard pending property generations when a session stops", function()
		local session, part = propertySession("Part")
		part.Size = Vector3.new(5, 2, 2)
		session:__queueChange(part, "Size")
		expect(next(session.__localPropertyEdits) ~= nil).to.equal(true)
		session:stop()
		expect(session.__localPropertyEdits).to.equal(nil)
		part:Destroy()
	end)

	it("should retain the first local change cursor until the batch is flushed", function()
		local cursor = 0
		local batcher = {
			__pendingPropertyChanges = {},
			isEmpty = function(self)
				return next(self.__pendingPropertyChanges) == nil
			end,
			add = function(self, instance, propertyName)
				self.__pendingPropertyChanges[instance] = propertyName
			end,
		}
		local session = sessionState({
			__apiContext = {
				getMessageCursor = function()
					return cursor
				end,
			},
			__changeBatcher = batcher,
		}, ServeSession)
		session:__queueChange("Part", "Transparency")
		cursor = 5
		session:__queueChange("Part", "CanCollide")
		expect(session.__pendingWriteCursor).to.equal(0)
		table.clear(batcher.__pendingPropertyChanges)
		session:__queueChange("Part", "Transparency")
		expect(session.__pendingWriteCursor).to.equal(5)
	end)

	it("should classify saved writes", function()
		expect(ServeSession.getWriteStatus(writeResult())).to.equal("Saved")
	end)

	it("should classify every conflict shape", function()
		for _, key in { "removed_instances", "added_instances", "updated_instances" } do
			local result = writeResult()
			table.insert(result.conflicts[key], {})
			expect(ServeSession.getWriteStatus(result)).to.equal("Conflicted")
		end
	end)

	it("should classify errors as failed even with conflicts", function()
		local result = writeResult({ { message = "Write failed" } })
		table.insert(result.conflicts.added_instances, {})
		expect(ServeSession.getWriteStatus(result)).to.equal("Failed")
	end)

	-- The setting is read for each write, so a change mid-session applies to
	-- the next write and is recorded with it.
	it("should send and record the authority set when each write starts", function()
		local Settings = require(script.Parent.Settings)
		local original = Settings:get("twoWaySyncAuthority")
		local success, err = pcall(function()
			local sent = {}
			local session = sessionState({
				__apiContext = {
					write = function(_self, _patch, _cursor, authority)
						table.insert(sent, authority)
						return Promise.resolve(writeResult())
					end,
				},
				__syncResults = {},
			}, ServeSession)
			for _, authority in { "Studio", "Filesystem", "Studio" } do
				Settings:set("twoWaySyncAuthority", authority)
				session:__writePatch({ updated = {}, added = {}, removed = {} }):await()
			end
			expect(table.concat(sent, ",")).to.equal("Studio,Filesystem,Studio")
			local results = session:getSyncResults()
			expect(results[1].authority).to.equal("Studio")
			expect(results[2].authority).to.equal("Filesystem")
		end)
		Settings:set("twoWaySyncAuthority", original)
		assert(success, err)
	end)

	-- A live edit session pins the authority, so a Studio edit still loses to
	-- the filesystem even when the user's setting says otherwise.
	it("should send and record a pinned authority instead of the setting", function()
		local Settings = require(script.Parent.Settings)
		local original = Settings:get("twoWaySyncAuthority")
		local success, err = pcall(function()
			Settings:set("twoWaySyncAuthority", "Studio")
			local sent = {}
			local session = sessionState({
				__twoWaySyncAuthority = "Filesystem",
				__apiContext = {
					write = function(_self, _patch, _cursor, authority)
						table.insert(sent, authority)
						return Promise.resolve(writeResult())
					end,
				},
				__syncResults = {},
			}, ServeSession)
			Settings:set("twoWaySyncAuthority", "Studio")
			session:__writePatch({ updated = {}, added = {}, removed = {} }):await()
			Settings:set("twoWaySyncAuthority", "Filesystem")
			session:__writePatch({ updated = {}, added = {}, removed = {} }):await()
			expect(table.concat(sent, ",")).to.equal("Filesystem,Filesystem")
			local results = session:getSyncResults()
			expect(results[1].authority).to.equal("Filesystem")
			expect(results[2].authority).to.equal("Filesystem")
		end)
		Settings:set("twoWaySyncAuthority", original)
		assert(success, err)
	end)

	-- A session that records what reaches the change batcher.
	local function changeSession(twoWaySync, deferring)
		local queued = {}
		local session = sessionState({
			__twoWaySync = twoWaySync,
			__deferredChanges = if deferring then {} else nil,
			__changeBatcher = {
				__pendingPropertyChanges = {},
				isEmpty = function()
					return #queued == 0
				end,
				add = function(_self, instance, propertyName)
					table.insert(queued, instance.Name .. "." .. propertyName)
				end,
				addChild = function(_self, instance)
					table.insert(queued, instance.Name .. ".Added")
				end,
			},
			__apiContext = {
				getMessageCursor = function()
					return 1
				end,
			},
		}, ServeSession)
		return session, queued
	end

	-- New instances follow the same rules as edits, and keep their order
	-- among them when held during the initial sync.
	it("should drop, hold, or queue a Studio-created instance in order", function()
		local created = Instance.new("Folder")
		created.Name = "Created"
		local edited = Instance.new("Folder")
		edited.Name = "Edited"
		for _, twoWaySync in { false, true } do
			for _, deferring in { false, true } do
				local session, queued = changeSession(twoWaySync, deferring)
				session:__onInstanceChanged(edited, "Name")
				session:__onChildAdded(created)
				if deferring then
					expect(#queued).to.equal(0)
					session:__releaseDeferredChanges()
				end
				local expected = if twoWaySync then "Edited.Name,Created.Added" else ""
				expect(table.concat(queued, ",")).to.equal(expected)
			end
		end
		created:Destroy()
		edited:Destroy()
	end)

	-- A rejected addition stops being tracked, root and all, and only the
	-- additions the server rejected are affected. A failed write rejects all.
	it("should stop tracking only the additions the server rejected", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		local PatchSet = require(script.Parent.PatchSet)
		for _, outcome in { "rejectFirst", "rejectChild", "transportFailure", "propertyError", "accepted" } do
			local instanceMap = InstanceMap.new()
			for _, id in { "A", "A_CHILD", "B" } do
				instanceMap:insert(id, Instance.new("Folder"))
			end
			local session = sessionState({ __instanceMap = instanceMap }, ServeSession)
			local patch = PatchSet.newEmpty()
			patch.added = {
				A = { ids = { id = "A", children = { { id = "A_CHILD", children = {} } } } },
				B = { ids = { id = "B", children = {} } },
			}
			local result = if outcome == "transportFailure"
				then nil
				else {
					errors = if outcome == "rejectFirst"
						then { { id = "A" } }
						elseif outcome == "rejectChild" then { { id = "A_CHILD" } }
						elseif outcome == "propertyError" then { { id = "A", property = "Value" } }
						else {},
				}
			session:__untrackRejectedAdditions(patch, result)

			local expectedGone = ({
				rejectFirst = { A = true, A_CHILD = true },
				rejectChild = { A_CHILD = true },
				transportFailure = { A = true, A_CHILD = true, B = true },
				propertyError = {},
				accepted = {},
			})[outcome]
			for _, id in { "A", "A_CHILD", "B" } do
				expect(instanceMap.fromIds[id] == nil).to.equal(expectedGone[id] == true)
			end
			instanceMap:stop()
		end
	end)

	-- A deletion the server saved stops tracking the instance and everything
	-- under it, so an undo sends it back as created. Others stay tracked.
	-- An undo that came back before the response is created again. Walks
	-- every combination of the deletion being saved and already undone.
	it("should stop tracking only the deletions the server saved", function()
		local InstanceMap = require(script.Parent.InstanceMap)
		for _, saved in { false, true } do
			for _, undone in { false, true } do
				local instanceMap = InstanceMap.new()
				local deleted = Instance.new("Folder")
				local child = Instance.new("Folder")
				child.Parent = deleted
				local kept = Instance.new("Folder")
				instanceMap:insert("DELETED", deleted)
				instanceMap:insert("CHILD", child)
				instanceMap:insert("KEPT", kept)
				if undone then
					deleted.Parent = kept
				end
				local queued = {}
				local session = sessionState({
					__instanceMap = instanceMap,
					__markBatchStart = function() end,
					__changeBatcher = {
						addChild = function(_self, instance)
							table.insert(queued, instance)
						end,
					},
				}, ServeSession)

				session:__untrackSavedRemovals({
					accepted = {
						removed_instances = if saved then { "DELETED" } else {},
						added_instances = {},
						updated_instances = {},
					},
				})

				expect(instanceMap.fromIds.DELETED == nil).to.equal(saved)
				expect(instanceMap.fromInstances[child] == nil).to.equal(saved)
				expect(instanceMap.fromIds.KEPT).to.equal(kept)
				expect(#queued).to.equal(if saved and undone then 1 else 0)
				instanceMap:stop()
				deleted:Destroy()
				kept:Destroy()
			end
		end
	end)

	-- Every combination of two-way sync and the initial sync still running.
	-- An edit is dropped without two-way sync, held while the place is only
	-- partly known, and queued otherwise.
	it("should drop, hold, or queue a Studio edit", function()
		local instance = Instance.new("Folder")
		instance.Name = "Edited"
		for _, twoWaySync in { false, true } do
			for _, deferring in { false, true } do
				local session, queued = changeSession(twoWaySync, deferring)
				session:__onInstanceChanged(instance, "Name")

				local held = if session.__deferredChanges then #session.__deferredChanges else 0
				expect(#queued).to.equal(if twoWaySync and not deferring then 1 else 0)
				expect(held).to.equal(if twoWaySync and deferring then 1 else 0)
			end
		end
		instance:Destroy()
	end)

	it("should release held edits in order and queue later ones directly", function()
		local first = Instance.new("Folder")
		first.Name = "First"
		local second = Instance.new("Folder")
		second.Name = "Second"
		local session, queued = changeSession(true, true)

		session:__onInstanceChanged(first, "Parent")
		session:__onInstanceChanged(second, "Name")
		session:__onInstanceChanged(first, "Name")
		expect(#queued).to.equal(0)

		session:__releaseDeferredChanges()
		expect(table.concat(queued, ",")).to.equal("First.Parent,Second.Name,First.Name")
		expect(session.__deferredChanges).to.equal(nil)

		session:__onInstanceChanged(second, "Parent")
		expect(queued[4]).to.equal("Second.Parent")

		-- Releasing again must not replay anything.
		session:__releaseDeferredChanges()
		expect(#queued).to.equal(4)
		first:Destroy()
		second:Destroy()
	end)

	-- Every combination of the budget having yielded and the session having
	-- stopped. The walk only has to stop when both are true, since a session
	-- can't be stopped without a yield to stop it in.
	it("should cancel the initial sync only after a yield in a stopped session", function()
		for _, yielded in { false, true } do
			for _, status in { "Connecting", "Connected", "Disconnected" } do
				local session = sessionState({ __status = status }, ServeSession)
				local step = session:__syncStep(function()
					return yielded
				end)
				local ok, err = pcall(step)
				local shouldCancel = yielded and status == "Disconnected"
				expect(ok).to.equal(not shouldCancel)
				if shouldCancel then
					expect(err).to.equal(ServeSession.SYNC_CANCELLED)
				end
			end
		end
	end)

	-- Stopping the session while hydration is suspended must end the sync
	-- there: no diff, no confirmation, no patch, and no held edits sent.
	it("should abandon the initial sync when the session stops mid-walk", function()
		local reached = {}
		local queued = {}
		local session
		session = sessionState({
			__status = "Connecting",
			__twoWaySync = true,
			__deferredChanges = { { instance = Instance.new("Folder"), propertyName = "Name" } },
			__updateLoadingText = function() end,
			__syncStep = function(self)
				-- Stands in for a budget that yields on the first step.
				return ServeSession.__syncStep(self, function()
					return true
				end)
			end,
			__userConfirmCallback = function()
				table.insert(reached, "confirm")
				return "Accept"
			end,
			__changeBatcher = {
				__pendingPropertyChanges = {},
				add = function()
					table.insert(queued, "edit")
				end,
			},
			__reconciler = {
				hydrate = function(_self, _instances, _rootId, _root, step)
					table.insert(reached, "hydrate")
					session.__status = "Disconnected"
					step()
					table.insert(reached, "hydrate finished")
				end,
				diff = function()
					table.insert(reached, "diff")
					return true, { removed = {}, added = {}, updated = {} }
				end,
			},
			__apiContext = {
				read = function()
					return Promise.resolve({ messageCursor = 1, instances = {} })
				end,
				setMessageCursor = function() end,
				getMessageCursor = function()
					return 1
				end,
			},
		}, ServeSession)

		local ok, err = session:__initialSync({ rootInstanceId = "ROOT" }):await()
		expect(ok).to.equal(false)
		expect(string.find(tostring(err), ServeSession.SYNC_CANCELLED, 1, true) ~= nil).to.equal(true)
		expect(table.concat(reached, ",")).to.equal("hydrate")
		expect(#queued).to.equal(0)
		session.__deferredChanges[1].instance:Destroy()
	end)
end
