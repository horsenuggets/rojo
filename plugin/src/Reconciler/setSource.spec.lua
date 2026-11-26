--[[
	Tests Source writes with a deterministic editor and scheduler. Proposals, version
	rejections, commits, and async returns are separate steps, without Studio services.
]]

return function()
	local createSourceSetter = require(script.Parent.setSource)

	local function signal()
		local callbacks = {}
		local event = { Count = 0 }
		function event:Connect(callback)
			local connection = {}
			callbacks[connection] = callback
			self.Count += 1
			function connection.Disconnect()
				if callbacks[connection] then
					callbacks[connection] = nil
					event.Count -= 1
				end
			end
			return connection
		end
		function event:Fire(...)
			for _, callback in table.clone(callbacks) do
				callback(...)
			end
		end
		return event
	end

	local function fixture(eager, serviceBoundary)
		local documents = signal()
		local ready = {}
		local errors = {}
		local scripts = {}
		local resume
		local editor = {}
		local test = { Documents = documents, Errors = errors }

		local function notify(instance)
			if serviceBoundary then
				if instance.Open then
					documents:Fire({
						GetScript = function()
							return instance
						end,
					})
				else
					instance.SourceChanged:Fire()
				end
			else
				for _, changed in table.clone(instance.Changes) do
					changed()
				end
			end
		end

		function editor.GetEditorSource(instance)
			assert(not instance.Destroyed, "Script was destroyed.")
			if instance.ReadError then
				error(instance.ReadError, 0)
			end
			return if instance.Open then instance.Text else instance.Source
		end

		function editor.ConnectChanged(instance, callback)
			local connection = {}
			instance.Changes[connection] = callback
			function connection.Disconnect()
				instance.Changes[connection] = nil
			end
			return connection
		end

		function editor.Spawn(callback)
			local thread = coroutine.create(callback)
			if eager then
				resume(thread)
			else
				table.insert(ready, thread)
			end
		end

		function editor.UpdateSourceAsync(instance, callback)
			assert(not instance.Destroyed, "Script was destroyed.")
			local operation = { Thread = coroutine.running() }
			instance.Operation = operation
			while true do
				local action, contents = coroutine.yield()
				if action == "attempt" then
					-- The engine may supply stale Source, not the open draft.
					operation.Proposal = callback(contents or instance.Source)
					operation.Version = instance.Version
					if operation.Proposal == nil then
						instance.Operation = nil
						return
					end
				elseif action == "commit" then
					assert(operation.Proposal ~= nil, "No proposal to commit.")
					if operation.Version == instance.Version then
						instance.Source = operation.Proposal
						instance.Text = operation.Proposal
						instance.Version += 1
						if not instance.DelayNotification then
							notify(instance)
						end
						if instance.DelayReturn then
							operation.Committed = true
							coroutine.yield()
						end
						instance.Operation = nil
						return
					end
					-- The rejected proposal did not touch the editor. Retry explicitly.
					operation.Proposal = nil
				elseif action == "fail" then
					instance.Operation = nil
					error(contents, 0)
				else
					error("Unexpected editor action.")
				end
			end
		end

		function resume(thread, ...)
			local success, err = coroutine.resume(thread, ...)
			if not success then
				table.insert(errors, tostring(err))
				for _, instance in scripts do
					if instance.Operation and instance.Operation.Thread == thread then
						instance.Operation = nil
					end
				end
			end
		end

		if serviceBoundary then
			test.Set = createSourceSetter.fromService({
				TextDocumentDidChange = documents,
				GetEditorSource = function(_, instance)
					return editor.GetEditorSource(instance)
				end,
				FindScriptDocument = function(_, instance)
					return if instance.Open then instance.Document else nil
				end,
				UpdateSourceAsync = function(_, instance, callback)
					return editor.UpdateSourceAsync(instance, callback)
				end,
			}, editor.Spawn)
		else
			test.Set = createSourceSetter(editor)
		end
		test.Text = editor.GetEditorSource
		test.Notify = notify

		function test.Start()
			while #ready > 0 do
				resume(table.remove(ready, 1))
			end
		end

		function test.Step(instance, action, contents)
			assert(instance.Operation, "No pending editor operation.")
			resume(instance.Operation.Thread, action, contents)
		end

		function test.Drain()
			test.Start()
			for _, instance in scripts do
				while instance.Operation do
					local operation = instance.Operation
					if operation.Committed then
						test.Step(instance, "finish")
					else
						test.Step(instance, "attempt")
						if instance.Operation == operation then
							test.Step(instance, "commit")
						end
					end
				end
			end
			test.Start()
		end

		function test.Map(instance)
			local InstanceMap = require(script.Parent.Parent.InstanceMap)
			local reports, outcomes = {}, {}
			local map = InstanceMap.new(function(changed, property, observation)
				table.insert(reports, { Instance = changed, Property = property, Observation = observation })
			end, nil, test.Set)
			map.__isGameRunning = function()
				return false
			end
			map.onSourceOutcome = function(changed, outcome)
				table.insert(outcomes, { Instance = changed, Outcome = outcome })
			end
			map:insert("SCRIPT", instance)
			return map, reports, outcomes
		end

		function test.Script(initial, open)
			local listeners = {}
			local instance = {
				AttributeChanged = signal(),
				Changed = signal(),
				ChildAdded = signal(),
				ClassName = "ModuleScript",
				IsA = function(_, className)
					return className == "LuaSourceContainer"
				end,
				GetFullName = function()
					return "Script"
				end,
				Changes = {},
				Destroying = {},
				Open = open ~= false,
				Source = initial,
				SourceChanged = signal(),
				GetPropertyChangedSignal = function(self)
					return self.SourceChanged
				end,
				Text = initial,
				Version = 0,
			}
			local document = {}
			instance.Document = document
			function document:GetText()
				return editor.GetEditorSource(instance)
			end
			function document:EditTextAsync(text, startLine, startCharacter, endLine, endCharacter)
				local current = instance.Text
				local function offset(line, character)
					local cursor = 1
					for _ = 2, line do
						cursor = assert(string.find(current, "\n", cursor, true)) + 1
					end
					return cursor + character - 1
				end
				local proposed = string.sub(current, 1, offset(startLine, startCharacter) - 1)
					.. text
					.. string.sub(current, offset(endLine, endCharacter))
				local operation = { Thread = coroutine.running(), Proposal = proposed, Version = instance.Version }
				instance.Operation = operation
				while true do
					local action, message = coroutine.yield()
					if action == "attempt" then
						if instance.CloseOnAttempt then
							instance.CloseOnAttempt, instance.Open = nil, false
						elseif instance.ReplaceOnAttempt then
							instance.ReplaceOnAttempt = nil
							instance.Document = table.clone(document)
						end
						if not instance.Open or instance.Document ~= self or instance.RejectRange then
							instance.Operation = nil
							return false, "Editor rejected the range."
						end
						-- A range request already owns its guarded proposal. A rejected editor
						-- attempt returns to the adapter instead of calling a string callback.
						if operation.Version ~= instance.Version then
							instance.Operation = nil
							return false, "Editor version changed."
						end
					elseif action == "commit" then
						if not instance.Open or instance.Document ~= self or operation.Version ~= instance.Version then
							instance.Operation = nil
							return false, "Editor document or version changed."
						end
						instance.Text, instance.Source = proposed, proposed
						instance.Version += 1
						if not instance.DelayNotification then
							notify(instance)
						end
						if instance.DelayReturn then
							operation.Committed = true
							coroutine.yield()
						end
						instance.Operation = nil
						return true
					elseif action == "retry" then
						instance.Operation = nil
						return false, "Editor rejected the version."
					elseif action == "fail" then
						instance.Operation = nil
						error(message, 0)
					else
						error("Unexpected editor action.")
					end
				end
			end
			function instance.Destroying.Connect(_, callback)
				local connection = {}
				listeners[connection] = callback
				function connection.Disconnect()
					listeners[connection] = nil
				end
				return connection
			end
			function instance:Destroy()
				self.Destroyed = true
				for _, callback in table.clone(listeners) do
					callback()
				end
			end
			function instance:Edit(contents)
				self.Text = contents
				if not self.Open then
					self.Source = contents
				end
				self.Version += 1
				notify(self)
			end
			table.insert(scripts, instance)
			return instance
		end

		return test
	end

	it("observes a commit once when its notification is delivered after return", function()
		for _, serviceBoundary in { false, true } do
			for _, open in { false, true } do
				for _, delayed in { false, true } do
					local test = fixture(false, serviceBoundary)
					local instance = test.Script("initial", open)
					instance.DelayNotification = delayed
					local map, reports, outcomes = test.Map(instance)
					local request = map:setSource(instance, "remote")
					test.Start()
					test.Step(instance, "attempt")
					test.Step(instance, "commit")
					expect(request.Status).to.equal("committed")
					expect(#reports).to.equal(1)
					expect(reports[1].Observation.Operation).to.equal(request)
					expect(request.CommittedRevision).to.equal(reports[1].Observation.Revision)
					if delayed then
						test.Notify(instance)
					end
					expect(#reports).to.equal(1)
					expect(map:getSourceRevision(instance)).to.equal(request.CommittedRevision)
					-- Only the owed delivery is absorbed. Later identical activity is typing.
					test.Notify(instance)
					expect(#reports).to.equal(2)
					expect(reports[2].Observation.Operation).to.equal(nil)
					expect(reports[2].Observation.Value).to.equal("remote")
					expect(#outcomes).to.equal(1)
					map:stop()
				end
			end
		end
	end)

	it("keeps a queued patch valid when the preceding commit notification is late", function()
		for _, open in { false, true } do
			local test = fixture(false, true)
			local instance = test.Script("initial", open)
			instance.DelayNotification = true
			local map, reports = test.Map(instance)
			local first = map:setSource(instance, "first")
			test.Drain()
			local second = map:setSource(instance, "second")
			test.Notify(instance)
			test.Drain()
			test.Notify(instance)
			expect(first.Status).to.equal("committed")
			expect(second.Status).to.equal("committed")
			expect(test.Text(instance)).to.equal("second")
			expect(#reports).to.equal(2)
			expect(reports[1].Observation.Operation).to.equal(first)
			expect(reports[2].Observation.Operation).to.equal(second)
			map:stop()
		end
	end)

	it("absorbs one late notification per commit when two commits return first", function()
		for _, open in { false, true } do
			local test = fixture(false, true)
			local instance = test.Script("initial", open)
			instance.DelayNotification = true
			local map, reports = test.Map(instance)
			local first = map:setSource(instance, "first")
			test.Drain()
			local second = map:setSource(instance, "second")
			test.Drain()
			-- Both deferred notifications arrive afterwards and read the latest text.
			test.Notify(instance)
			test.Notify(instance)
			expect(first.Status).to.equal("committed")
			expect(second.Status).to.equal("committed")
			expect(#reports).to.equal(2)
			expect(reports[1].Observation.Operation).to.equal(first)
			expect(reports[2].Observation.Operation).to.equal(second)
			-- The debts are spent, so any further identical activity is typing.
			test.Notify(instance)
			expect(#reports).to.equal(3)
			expect(reports[3].Observation.Operation).to.equal(nil)
			map:stop()
		end
	end)

	it("retains typing and exact reversions after a commit with delayed notification", function()
		for _, open in { false, true } do
			local test = fixture(false, true)
			local instance = test.Script("initial", open)
			instance.DelayNotification = true
			local map, reports = test.Map(instance)
			local request = map:setSource(instance, "remote")
			test.Drain()
			expect(#reports).to.equal(1)
			instance:Edit("genuine draft")
			instance:Edit("remote")
			test.Notify(instance)
			expect(request.Status).to.equal("committed")
			-- Intervening typing revokes the owed delivery, so the late callback is kept.
			expect(#reports).to.equal(4)
			for index, value in { "genuine draft", "remote", "remote" } do
				expect(reports[index + 1].Observation.Operation).to.equal(nil)
				expect(reports[index + 1].Observation.Value).to.equal(value)
			end
			map:stop()
		end
	end)

	it("does not retain successful notification ownership across observer leases", function()
		local test = fixture(false, true)
		local instance = test.Script("initial")
		instance.DelayNotification = true
		local oldMap = test.Map(instance)
		local request = oldMap:setSource(instance, "remote")
		test.Drain()
		oldMap:stop()
		local freshMap, reports = test.Map(instance)
		test.Notify(instance)
		expect(request.Status).to.equal("committed")
		expect(#reports).to.equal(1)
		expect(reports[1].Observation.Operation).to.equal(nil)
		freshMap:stop()
	end)

	it("does not normalize a failed invocation into a successful observed commit", function()
		for _, open in { false, true } do
			local test = fixture(false, true)
			local instance = test.Script("initial", open)
			instance.DelayNotification, instance.DelayReturn = true, true
			local map, reports = test.Map(instance)
			local request = map:setSource(instance, "remote")
			test.Start()
			test.Step(instance, "attempt")
			test.Step(instance, "fail", "Uncertain editor failure.")
			expect(request.Status).to.equal("failed")
			expect(request.CommittedRevision).to.equal(nil)
			expect(#reports).to.equal(0)
			instance:Edit("remote")
			expect(#reports).to.equal(1)
			expect(reports[1].Observation.Operation).to.equal(nil)
			map:stop()
		end
	end)

	it("revalidates a rejected range after newer local text instead of reusing its offsets", function()
		local test = fixture(false, true)
		local instance = test.Script("before\n-- é\u{1F642}")
		local map = test.Map(instance)
		local request = map:setSource(instance, "after\n-- é\u{1F642}")
		test.Start()
		instance:Edit("before\n-- newer é\u{1F642}")
		test.Step(instance, "attempt")
		expect(request.Status).to.equal("locally-cancelled")
		expect(request.CommitUncertain).to.equal(false)
		expect(test.Text(instance)).to.equal("before\n-- newer é\u{1F642}")
		map:stop()
	end)

	it("reports unchanged range rejection without spinning or claiming a commit", function()
		local test = fixture(false, true)
		local instance = test.Script("before")
		local map = test.Map(instance)
		instance.RejectRange = true
		local request = map:setSource(instance, "after")
		test.Start()
		test.Step(instance, "attempt")
		expect(request.Status).to.equal("failed")
		expect(request.Error:find("Editor rejected the range.", 1, true) ~= nil).to.equal(true)
		expect(test.Text(instance)).to.equal("before")
		expect(instance.Operation).to.equal(nil)
		map:stop()
	end)

	it("revalidates document replacement before retrying the exact range", function()
		local test = fixture(false, true)
		local instance = test.Script("before\n-- draft")
		local map = test.Map(instance)
		instance.ReplaceOnAttempt = true
		local request = map:setSource(instance, "after\n-- draft")
		test.Drain()
		expect(request.Status).to.equal("committed")
		expect(test.Text(instance)).to.equal("after\n-- draft")
		map:stop()
	end)

	it("retains a newer closed-source edit after an acquired range document closes", function()
		local test = fixture(false, true)
		local instance = test.Script("before")
		local map = test.Map(instance)
		local request = map:setSource(instance, "after")
		test.Start()
		test.Step(instance, "attempt")
		instance.Open = false
		instance:Edit("closed draft")
		test.Step(instance, "commit")
		test.Drain()
		expect(request.Status).to.equal("locally-cancelled")
		expect(test.Text(instance)).to.equal("closed draft")
		map:stop()
	end)

	it("uses the guarded closed-script boundary when an open document closes", function()
		local test = fixture(false, true)
		local instance = test.Script("before")
		local map = test.Map(instance)
		instance.CloseOnAttempt = true
		local request = map:setSource(instance, "after")
		test.Drain()
		expect(request.Status).to.equal("committed")
		expect(instance.Open).to.equal(false)
		expect(test.Text(instance)).to.equal("after")
		map:stop()
	end)

	it("holds cancelled invocations through their actual return across session replacement", function()
		for _, open in { false, true } do
			local test = fixture(false, true)
			local instance = test.Script("initial", open)
			instance.DelayReturn = true
			local oldMap, oldReports, oldOutcomes = test.Map(instance)
			local obsolete = oldMap:setSource(instance, "obsolete")
			test.Start()
			test.Step(instance, "attempt")
			local invocation = instance.Operation
			oldMap:stop()
			expect(obsolete.Status).to.equal("pending")
			expect(obsolete.CancellationRequested).to.equal(true)
			expect(obsolete.CommitUncertain).to.equal(true)
			expect(#oldOutcomes).to.equal(0)
			local freshMap, freshReports, freshOutcomes = test.Map(instance)
			local fresh = freshMap:setSource(instance, "fresh")
			test.Start()
			expect(instance.Operation).to.equal(invocation)
			test.Step(instance, "commit")
			expect(obsolete.Status).to.equal("pending")
			expect(freshReports[1].Observation.Operation).to.equal(obsolete)
			expect(#oldReports).to.equal(0)
			test.Step(instance, "finish")
			expect(obsolete.Status).to.equal("committed")
			expect(obsolete.CommitUncertain).to.equal(false)
			expect(obsolete.CommittedRevision).to.equal(freshReports[1].Observation.Revision)
			expect(#oldOutcomes).to.equal(1)
			expect(freshOutcomes[1].Outcome).to.equal(obsolete)
			test.Drain()
			expect(fresh.Status).to.equal("committed")
			expect(test.Text(instance)).to.equal("fresh")
			freshMap:stop()
			expect(test.Documents.Count).to.equal(0)
		end
	end)

	it("preserves new-generation edits and reversions throughout cancelled drains", function()
		for _, moment in { "before-commit", "after-commit", "after-return" } do
			for _, revert in { false, true } do
				local test = fixture(false, true)
				local instance = test.Script("initial")
				instance.DelayReturn = true
				local oldMap = test.Map(instance)
				local obsolete = oldMap:setSource(instance, "obsolete")
				test.Start()
				test.Step(instance, "attempt")
				local freshMap, reports = test.Map(instance)
				local fresh = freshMap:setSource(instance, "fresh")
				local expected
				local function edit()
					local previous = test.Text(instance)
					instance:Edit("genuine draft")
					expected = "genuine draft"
					if revert then
						instance:Edit(previous)
						expected = previous
					end
					expect(reports[#reports].Observation.Operation).to.equal(nil)
				end
				if moment == "before-commit" then
					edit()
				end
				test.Step(instance, "commit")
				if moment ~= "before-commit" then
					if moment == "after-commit" then
						edit()
					end
					test.Step(instance, "finish")
					if moment == "after-return" then
						edit()
					end
				end
				test.Drain()
				expect(obsolete.CommitUncertain).to.equal(false)
				expect(fresh.Status).to.equal("locally-cancelled")
				expect(test.Text(instance)).to.equal(expected)
				oldMap:stop()
				freshMap:stop()
			end
		end
	end)

	it("isolates changed, scheduled, completed, and supplied callback failures", function()
		for _, failing in { "changed", "scheduled", "completed", "callback" } do
			local test = fixture()
			local instance = test.Script("initial")
			local outcomes, calls = {}, 0
			local function fail()
				error("Consumer failure.", 0)
			end
			local connection = test.Set.Observe(
				instance,
				if failing == "changed" then fail else function() end,
				function(request)
					table.insert(outcomes, request)
					if failing == "completed" then
						fail()
					end
				end,
				if failing == "scheduled" then fail else nil
			)
			local first = test.Set(instance, "first", function()
				calls += 1
				if failing == "callback" then
					fail()
				end
			end)
			test.Start()
			test.Step(instance, "attempt")
			local second = test.Set(instance, "second")
			test.Step(instance, "commit")
			test.Drain()
			expect(first.Status).to.equal("committed")
			expect(second.Status).to.equal("committed")
			expect(test.Text(instance)).to.equal("second")
			expect(#outcomes).to.equal(2)
			expect(calls).to.equal(1)
			expect(#test.Errors > 0).to.equal(true)
			expect(#first.ConsumerErrors).to.equal(1)
			expect(first.ConsumerErrors[1]:find("Consumer failure.", 1, true) ~= nil).to.equal(true)
			expect(first.Error).to.equal(nil)
			local third = test.Set(instance, "third")
			test.Drain()
			expect(third.Status).to.equal("committed")
			connection:Disconnect()
		end
	end)

	it("finishes cancellation despite throwing reentrant callbacks and drains into a replacement", function()
		local test = fixture(false, true)
		local instance = test.Script("initial")
		local calls = 0
		local old = test.Set.Observe(instance, function() end, function()
			error("Cancellation observer failure.", 0)
		end)
		local active = test.Set(instance, "obsolete")
		test.Start()
		test.Step(instance, "attempt")
		local fresh, replacement
		local queued = test.Set(instance, "queued", function()
			calls += 1
			replacement = test.Set.Observe(instance, function() end)
			fresh = test.Set(instance, "fresh")
			error("Cancellation callback failure.", 0)
		end)
		old:Disconnect()
		expect(queued.Status).to.equal("locally-cancelled")
		expect(calls).to.equal(1)
		test.Step(instance, "commit")
		test.Drain()
		expect(active.Status).to.equal("committed")
		expect(fresh.Status).to.equal("committed")
		expect(test.Text(instance)).to.equal("fresh")
		expect(#test.Errors).to.equal(3)
		replacement:Disconnect()
	end)

	it("schedules successors set reentrantly from completion before reporting a throw", function()
		for _, replace in { false, true } do
			local test = fixture(true)
			local instance = test.Script("initial")
			local fresh, replacement
			local calls = 0
			local old = test.Set.Observe(instance, function() end)
			local first = test.Set(instance, "first", function()
				calls += 1
				if replace then
					replacement = test.Set.Observe(instance, function() end)
				end
				fresh = test.Set(instance, "fresh")
				error("Reentrant completion failure.", 0)
			end)
			test.Step(instance, "attempt")
			test.Step(instance, "commit")
			test.Drain()
			expect(first.Status).to.equal("committed")
			expect(calls).to.equal(1)
			expect(fresh.Status).to.equal("committed")
			expect(test.Text(instance)).to.equal("fresh")
			expect(#test.Errors).to.equal(1)
			old:Disconnect()
			if replacement then
				replacement:Disconnect()
			end
		end
	end)

	it("reports the editor failure when its terminal consumer also fails", function()
		local test = fixture()
		local instance = test.Script("initial")
		local calls = 0
		local failed = test.Set(instance, "failed", function()
			calls += 1
			error("Failure handler failed.", 0)
		end)
		test.Start()
		test.Step(instance, "attempt")
		local fresh = test.Set(instance, "fresh")
		test.Step(instance, "fail", "Editor permission failure.")
		test.Drain()
		expect(failed.Status).to.equal("failed")
		expect(failed.Error).to.equal("Editor permission failure.")
		expect(calls).to.equal(1)
		expect(fresh.Status).to.equal("committed")
		expect(#test.Errors).to.equal(1)
		expect(test.Errors[1]:find("Failure handler failed.", 1, true) ~= nil).to.equal(true)
		expect(test.Errors[1]:find("Editor permission failure.", 1, true) ~= nil).to.equal(true)
	end)

	it("keeps local intent when its change observer fails during an active proposal", function()
		local test = fixture()
		local instance = test.Script("initial")
		local connection = test.Set.Observe(instance, function()
			error("Local intent observer failure.", 0)
		end)
		local request = test.Set(instance, "remote")
		test.Start()
		test.Step(instance, "attempt")
		instance:Edit("draft")
		instance:Edit("initial")
		test.Drain()
		expect(request.Status).to.equal("locally-cancelled")
		expect(test.Text(instance)).to.equal("initial")
		expect(#test.Errors).to.equal(2)
		connection:Disconnect()
	end)

	it("isolates supersession and no-op callbacks that enqueue a successor and throw", function()
		for _, noChange in { false, true } do
			local test = fixture()
			local instance = test.Script("initial")
			local fresh, calls = nil, 0
			local first = test.Set(instance, if noChange then "initial" else "first", function()
				calls += 1
				fresh = test.Set(instance, "fresh")
				error("Immediate completion failure.", 0)
			end)
			local superseded
			if not noChange then
				superseded = test.Set(instance, "superseded")
				expect(superseded.Status).to.equal("superseded")
			end
			test.Drain()
			expect(first.Status).to.equal(if noChange then "committed" else "superseded")
			expect(calls).to.equal(1)
			expect(fresh.Status).to.equal("committed")
			expect(test.Text(instance)).to.equal("fresh")
			expect(#test.Errors).to.equal(1)
		end
	end)

	it("waits for a cancelled closed-script invocation before its engine callback proposes", function()
		local test = fixture(false, true)
		local instance = test.Script("initial", false)
		local oldMap, _, outcomes = test.Map(instance)
		local obsolete = oldMap:setSource(instance, "obsolete")
		test.Start()
		local invocation = instance.Operation
		oldMap:stop()
		local freshMap = test.Map(instance)
		local fresh = freshMap:setSource(instance, "fresh")
		test.Start()
		expect(instance.Operation).to.equal(invocation)
		expect(obsolete.Status).to.equal("pending")
		expect(obsolete.CommitUncertain).to.equal(false)
		test.Step(instance, "attempt")
		expect(obsolete.Status).to.equal("locally-cancelled")
		expect(#outcomes).to.equal(1)
		test.Drain()
		expect(fresh.Status).to.equal("committed")
		freshMap:stop()
	end)

	it("keeps a reentrant replacement lease when cancellation callbacks adopt the script", function()
		local test = fixture(false, true)
		local instance = test.Script("initial")
		local oldMap = test.Map(instance)
		oldMap:setSource(instance, "obsolete")
		test.Start()
		test.Step(instance, "attempt")
		local replacement, reports
		local queued = oldMap:setSource(instance, "queued", function()
			replacement, reports = test.Map(instance)
		end)
		local superseded = test.Map(instance)
		expect(queued.Status).to.equal("locally-cancelled")
		expect(superseded:getSourceRevision(instance)).to.equal(nil)
		local fresh = replacement:setSource(instance, "fresh")
		superseded:stop()
		test.Step(instance, "commit")
		test.Drain()
		expect(fresh.Status).to.equal("committed")
		expect(#reports).to.equal(2)
		expect(test.Text(instance)).to.equal("fresh")
		oldMap:stop()
		replacement:stop()
	end)

	it("keeps failed proposal drains uncertain without blocking a safe successor", function()
		local test = fixture(false, true)
		local instance = test.Script("initial")
		local oldMap, _, outcomes = test.Map(instance)
		local obsolete = oldMap:setSource(instance, "obsolete")
		test.Start()
		test.Step(instance, "attempt")
		oldMap:stop()
		local freshMap = test.Map(instance)
		local fresh = freshMap:setSource(instance, "fresh")
		test.Step(instance, "fail", "Uncertain engine failure.")
		expect(obsolete.Status).to.equal("failed")
		expect(obsolete.CommitUncertain).to.equal(true)
		expect(obsolete.CommittedRevision).to.equal(nil)
		expect(#outcomes).to.equal(1)
		test.Drain()
		expect(fresh.Status).to.equal("committed")
		freshMap:stop()
	end)

	it("drains 1000 seeded replacement traces with immediate edits and isolated consumers", function()
		for seed = 1, 1000 do
			local random = seed
			local function choose(limit)
				random = (random * 48271) % 2147483647
				return random % limit + 1
			end
			local test = fixture(false, true)
			local instance = test.Script("initial", seed % 2 == 0)
			local other = test.Script("other")
			instance.DelayReturn = true
			local expectedErrors = 0
			for round = 1, 8 do
				local initial = test.Text(instance)
				local oldMap, _, oldOutcomes = test.Map(instance)
				local obsolete = oldMap:setSource(instance, `obsolete {round}`)
				test.Start()
				test.Step(instance, "attempt")
				local invocation = instance.Operation
				if choose(2) == 1 then
					oldMap:stop()
				end
				local freshMap, reports, outcomes = test.Map(instance)
				local version = instance.Version
				local phase, revert = choose(4), choose(2) == 1
				local editValue = if choose(2) == 1 then obsolete.Value else `draft {round}`
				local localValue
				local function edit()
					local previous = test.Text(instance)
					instance:Edit(editValue)
					localValue = editValue
					if revert then
						instance:Edit(previous)
						localValue = previous
					end
				end
				if phase == 1 then
					edit()
				end
				local desired = if choose(2) == 1 then initial else `fresh {round}`
				local calls = 0
				local throws = choose(3) == 1
				local fresh = freshMap:setSource(instance, desired, function()
					calls += 1
					if throws then
						error("Seeded consumer failure.", 0)
					end
				end)
				if throws then
					expectedErrors += 1
				end
				-- An unrelated script must commit while the cancelled invocation owns its slot.
				local independent = test.Set(other, `other {round}`)
				test.Start()
				expect(instance.Operation).to.equal(invocation)
				test.Step(other, "attempt")
				test.Step(other, "commit")
				expect(independent.Status).to.equal("committed")
				if phase == 2 then
					edit()
				end
				test.Step(instance, "commit")
				if phase <= 2 then
					if not instance.Open then
						test.Step(instance, "attempt")
					end
					expect(obsolete.Status).to.equal("locally-cancelled")
				else
					expect(reports[1].Observation.Operation).to.equal(obsolete)
					if phase == 3 then
						edit()
					end
					test.Step(instance, "finish")
					expect(obsolete.Status).to.equal("committed")
					if phase == 4 then
						edit()
					end
				end
				test.Drain()
				expect(test.Text(instance)).to.equal(if phase == 1 then desired else localValue)
				expect(fresh.Status).to.equal(if phase == 1 then "committed" else "locally-cancelled")
				expect(obsolete.CommitUncertain).to.equal(false)
				expect(calls).to.equal(1)
				expect(#oldOutcomes).to.equal(1)
				expect(#outcomes).to.equal(2)
				expect(#reports).to.equal(instance.Version - version)
				oldMap:stop()
				expect(freshMap:getSourceRevision(instance) ~= nil).to.equal(true)
				freshMap:stop()
				expect(test.Documents.Count).to.equal(0)
			end
			expect(#test.Errors).to.equal(expectedErrors)
		end
	end)

	it("continuously reports open document edits and exact reversions even while paused", function()
		local test = fixture()
		local instance = test.Script("initial")
		local map, reports = test.Map(instance)
		map:pauseInstance(instance)
		instance:Edit("draft")
		instance:Edit("initial")
		expect(#reports).to.equal(2)
		expect(reports[1].Property).to.equal("Source")
		expect(reports[2].Observation.Revision).to.equal(reports[1].Observation.Revision + 1)
		map:stop()
		instance:Edit("after stop")
		expect(#reports).to.equal(2)
	end)

	it("reports delayed commits only at async completion, not at scheduling", function()
		local test = fixture()
		local instance = test.Script("initial")
		local map, reports, outcomes = test.Map(instance)
		local request = map:setSource(instance, "remote")
		expect(request.Status).to.equal("pending")
		expect(#outcomes).to.equal(0)
		test.Start()
		test.Step(instance, "attempt")
		map:unpauseAllInstances()
		test.Step(instance, "commit")
		expect(request.Status).to.equal("committed")
		expect(#outcomes).to.equal(1)
		-- Observations are immediate, never content-filtered. The outcome confirms
		-- exactly which observation belongs to this completed operation.
		expect(#reports).to.equal(1)
		expect(request.CommittedRevision).to.equal(reports[1].Observation.Revision)
		expect(reports[1].Observation.Operation).to.equal(request)
		instance:Edit("draft")
		instance:Edit("remote")
		expect(#reports).to.equal(3)
		expect(reports[3].Observation.Operation).to.equal(nil)
		map:stop()
	end)

	it("does not classify typing that matches an uncommitted proposal as a commit", function()
		local test = fixture()
		local instance = test.Script("initial")
		local map, reports, outcomes = test.Map(instance)
		local request = map:setSource(instance, "remote")
		test.Start()
		test.Step(instance, "attempt")
		instance:Edit("remote")
		expect(#reports).to.equal(1)
		test.Step(instance, "commit")
		test.Step(instance, "attempt")
		expect(request.Status).to.equal("locally-cancelled")
		expect(request.CommittedRevision).to.equal(nil)
		expect(#outcomes).to.equal(1)
		map:stop()
	end)

	it("invalidates queued and retrying callbacks on untrack and stop across generations", function()
		for _, stop in { false, true } do
			for _, started in { false, true } do
				local test = fixture()
				local instance = test.Script("initial")
				local map, reports, outcomes = test.Map(instance)
				local request = map:setSource(instance, "obsolete")
				if started then
					test.Start()
					test.Step(instance, "attempt")
				end
				if stop then
					map:stop()
				else
					map:removeId("SCRIPT")
				end
				expect(request.Status).to.equal(if started then "pending" else "locally-cancelled")
				expect(request.CommitUncertain == true).to.equal(started)
				expect(#outcomes).to.equal(if started then 0 else 1)
				if started then
					test.Step(instance, "attempt")
				end
				test.Drain()
				expect(request.Status).to.equal("locally-cancelled")
				expect(request.CommitUncertain).to.equal(false)
				expect(#outcomes).to.equal(1)
				expect(test.Text(instance)).to.equal("initial")
				expect(#reports).to.equal(0)
				local fresh, _, freshOutcomes = test.Map(instance)
				local nextRequest = fresh:setSource(instance, "fresh")
				test.Drain()
				expect(test.Text(instance)).to.equal("fresh")
				expect(nextRequest.Generation == request.Generation).to.equal(false)
				expect(#freshOutcomes).to.equal(1)
				fresh:stop()
			end
		end
	end)

	it("reports uncertainty when stop cannot revoke an engine-owned proposal", function()
		local test = fixture(false, true)
		local instance = test.Script("initial")
		local map, reports, outcomes = test.Map(instance)
		local request = map:setSource(instance, "remote")
		test.Start()
		test.Step(instance, "attempt")
		map:stop()
		expect(request.Status).to.equal("pending")
		expect(request.CancellationRequested).to.equal(true)
		expect(request.CommitUncertain).to.equal(true)
		expect(#outcomes).to.equal(0)
		-- Unlike a retry, an engine commit does not invoke our invalidated callback.
		test.Step(instance, "commit")
		expect(request.Status).to.equal("committed")
		expect(request.CommitUncertain).to.equal(false)
		expect(test.Text(instance)).to.equal("remote")
		expect(#outcomes).to.equal(1)
		expect(#reports).to.equal(0)
		expect(test.Documents.Count).to.equal(0)
	end)

	it("invalidates the old session when a new map adopts the same script", function()
		local test = fixture(false, true)
		local instance = test.Script("initial")
		local oldMap, oldReports, oldOutcomes = test.Map(instance)
		local obsolete = oldMap:setSource(instance, "obsolete")
		test.Start()
		test.Step(instance, "attempt")
		local freshMap, freshReports = test.Map(instance)
		expect(obsolete.Status).to.equal("pending")
		expect(obsolete.Reason).to.equal("superseded-session")
		expect(#oldOutcomes).to.equal(0)
		expect(oldMap:getSourceRevision(instance)).to.equal(nil)
		expect((pcall(oldMap.setSource, oldMap, instance, "stale session"))).to.equal(false)
		local fresh = freshMap:setSource(instance, "fresh")
		oldMap:stop()
		test.Step(instance, "retry")
		test.Drain()
		expect(fresh.Status).to.equal("committed")
		expect(test.Text(instance)).to.equal("fresh")
		expect(#oldReports).to.equal(0)
		expect(#freshReports).to.equal(1)
		expect(test.Documents.Count).to.equal(1)
		freshMap:stop()
		expect(test.Documents.Count).to.equal(0)
	end)

	it("reports failures and superseded queued writes exactly once", function()
		local test = fixture()
		local instance = test.Script("initial")
		local outcomes = {}
		local function completed(outcome)
			table.insert(outcomes, outcome)
		end
		local first = test.Set(instance, "first", completed)
		local second = test.Set(instance, "second", completed)
		expect(first.Status).to.equal("superseded")
		test.Start()
		test.Step(instance, "fail", "Editor failure.")
		expect(second.Status).to.equal("failed")
		expect(second.Error:find("Editor failure.", 1, true) ~= nil).to.equal(true)
		expect(#outcomes).to.equal(2)
		expect(#test.Errors).to.equal(0)
	end)

	it("shares one document dispatcher across idle scripts and uses closed Source fallback", function()
		local test = fixture(false, true)
		local first = test.Script("first")
		local second = test.Script("second", false)
		local firstMap, firstReports = test.Map(first)
		local secondMap, secondReports = test.Map(second)
		expect(test.Documents.Count).to.equal(1)
		first:Edit("open draft")
		second:Edit("closed draft")
		expect(#firstReports).to.equal(1)
		expect(#secondReports).to.equal(1)
		-- Source property notifications from an open document are duplicates.
		first.SourceChanged:Fire()
		expect(#firstReports).to.equal(1)
		first.Open = false
		first:Edit("closed now")
		second.Open = true
		second:Edit("open now")
		expect(#firstReports).to.equal(2)
		expect(#secondReports).to.equal(2)
		firstMap:stop()
		expect(test.Documents.Count).to.equal(1)
		secondMap:stop()
		expect(test.Documents.Count).to.equal(0)
		expect(first.SourceChanged.Count).to.equal(0)
		expect(second.SourceChanged.Count).to.equal(0)
	end)

	it("adopts reification operations and reports scheduling before any outcome", function()
		local test = fixture(true, true)
		local instance = test.Script("initial")
		local request = test.Set(instance, "reified")
		local map, _, outcomes = test.Map(instance)
		local order = {}
		map.onSourceScheduled = function(_, operation)
			table.insert(order, operation.Status)
		end
		test.Step(instance, "attempt")
		test.Step(instance, "commit")
		expect(outcomes[1].Outcome).to.equal(request)
		local same = map:setSource(instance, "reified")
		expect(order[1]).to.equal("pending")
		expect(same.Status).to.equal("committed")
		map:stop()
	end)

	it("cleans up when adopting a pending operation synchronously stops the map", function()
		local InstanceMap = require(script.Parent.Parent.InstanceMap)
		local test = fixture(false, true)
		local instance = test.Script("initial")
		local request = test.Set(instance, "remote")
		local map = InstanceMap.new(nil, nil, test.Set)
		map.onSourceScheduled = function()
			map:stop()
		end
		map:insert("SCRIPT", instance)
		test.Drain()
		expect(request.Status).to.equal("locally-cancelled")
		expect(map:size()).to.equal(0)
		expect((next(map.sourceConnections))).to.equal(nil)
		expect(test.Documents.Count).to.equal(0)
		expect(test.Text(instance)).to.equal("initial")
	end)

	it("does not erase local notifications while a committed call is still returning", function()
		local test = fixture(false, true)
		local instance = test.Script("initial")
		instance.DelayReturn = true
		local map, reports, outcomes = test.Map(instance)
		local request = map:setSource(instance, "remote")
		test.Start()
		test.Step(instance, "attempt")
		test.Step(instance, "commit")
		expect(request.Status).to.equal("pending")
		instance:Edit("typed")
		instance:Edit("remote")
		expect(#reports).to.equal(3)
		test.Step(instance, "finish")
		expect(request.Status).to.equal("committed")
		expect(request.CommittedRevision).to.equal(reports[1].Observation.Revision)
		expect(reports[3].Observation.Operation).to.equal(nil)
		expect(#outcomes).to.equal(1)
		map:stop()
	end)

	it("returns pending Source operations from real patch application and reports later failure", function()
		local applyPatch = require(script.Parent.applyPatch)
		local test = fixture(false, true)
		local instance = test.Script("initial")
		local map, reports, outcomes = test.Map(instance)
		local patch = {
			added = {},
			removed = {},
			updated = { { id = "SCRIPT", changedProperties = { Source = { String = "remote" } } } },
			sourceOperations = { SCRIPT = "server-operation" },
		}
		local unapplied, operations = applyPatch(map, patch)
		expect(#unapplied.updated).to.equal(0)
		expect(#operations).to.equal(1)
		expect(operations[1].id).to.equal("SCRIPT")
		expect(operations[1].operation.Status).to.equal("pending")
		expect(operations[1].operation.ServerOperation).to.equal("server-operation")
		expect(#outcomes).to.equal(0)
		test.Start()
		map:unpauseAllInstances()
		test.Step(instance, "fail", "Editor permission failure.")
		expect(outcomes[1].Outcome.Status).to.equal("failed")
		expect(#test.Errors).to.equal(0)
		local _, retry = applyPatch(map, patch)
		test.Start()
		test.Step(instance, "attempt")
		instance:Edit("draft")
		instance:Edit("initial")
		test.Step(instance, "commit")
		expect(retry[1].operation.Status).to.equal("locally-cancelled")
		expect(#reports).to.equal(2)
		expect(test.Text(instance)).to.equal("initial")
		map:stop()
	end)

	it("does not attribute a no-op acknowledgement to an existing local revision", function()
		local test = fixture(false, true)
		local instance = test.Script("initial")
		local map, reports = test.Map(instance)
		instance:Edit("draft")
		local request = map:setSource(instance, "draft")
		expect(request.Status).to.equal("committed")
		expect(request.NoChange).to.equal(true)
		expect(request.CommittedRevision).to.equal(nil)
		expect(#reports).to.equal(1)
		map:stop()
	end)

	it("still establishes local intent when a document notification cannot read Source", function()
		local test = fixture(false, true)
		local instance = test.Script("initial")
		local map, reports = test.Map(instance)
		local request = map:setSource(instance, "remote")
		instance.ReadError = "Temporary read failure."
		instance:Edit("draft")
		expect(#reports).to.equal(1)
		expect(reports[1].Observation.Error).to.equal("Temporary read failure.")
		instance.ReadError = nil
		test.Drain()
		expect(request.Status).to.equal("locally-cancelled")
		expect(test.Text(instance)).to.equal("draft")
		map:stop()
	end)

	it("matches editor state and every write notification across 1000 seeded bounded traces", function()
		for seed = 1, 1000 do
			local random = seed
			local function choose(limit)
				random = (random * 48271) % 2147483647
				return random % limit + 1
			end
			local test = fixture(false, true)
			local instance = test.Script("initial", seed % 2 == 0)
			instance.DelayReturn = seed % 3 == 0
			local map, reports, outcomes = test.Map(instance)
			local epoch, lastLocal = 0, "initial"
			local intent
			local requests = {}
			for _ = 1, 32 do
				local action = choose(7)
				if action == 1 then
					local value = "remote " .. tostring(choose(4))
					intent = { Epoch = epoch, Value = value }
					table.insert(requests, map:setSource(instance, value))
				elseif action == 2 or action == 3 then
					local previous = test.Text(instance)
					lastLocal = "local " .. tostring(choose(4))
					instance:Edit(lastLocal)
					epoch += 1
					if action == 3 then
						lastLocal = previous
						instance:Edit(lastLocal)
						epoch += 1
					end
				elseif action == 4 then
					test.Start()
				elseif action == 5 and instance.Operation then
					test.Step(instance, if instance.Operation.Committed then "finish" else "attempt")
				elseif action == 6 and instance.Operation then
					local operation = instance.Operation
					if operation.Committed then
						test.Step(instance, "finish")
					elseif operation.Proposal ~= nil then
						test.Step(instance, "commit")
					end
				elseif action == 7 then
					if choose(2) == 1 then
						map:pauseInstance(instance)
					else
						map:unpauseAllInstances()
					end
				end
				expect(#reports).to.equal(instance.Version)
			end
			test.Drain()
			local expected = if intent and intent.Epoch == epoch then intent.Value else lastLocal
			assert(test.Text(instance) == expected, "Editor oracle mismatch for seed " .. tostring(seed))
			expect(#reports).to.equal(instance.Version)
			expect(#outcomes).to.equal(#requests)
			for index, report in reports do
				expect(report.Observation.Revision).to.equal(index)
			end
			for _, request in requests do
				expect(request.Status == "pending").to.equal(false)
			end
			map:stop()
			expect(test.Documents.Count).to.equal(0)
			expect(#test.Errors).to.equal(0)
		end
	end)

	it("preserves a newer editor draft before the first callback, even with stale Source", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "older")
		instance:Edit("newer")
		test.Drain()
		expect(test.Text(instance)).to.equal("newer")
		expect(#test.Errors).to.equal(0)
	end)

	it("preserves a newer edit reverted to the exact scheduled text", function()
		for _, open in { false, true } do
			for _, started in { false, true } do
				local test = fixture()
				local instance = test.Script("initial", open)
				test.Set(instance, "older")
				if started then
					test.Start()
					test.Step(instance, "attempt")
				end
				instance:Edit("newer")
				instance:Edit("initial")
				test.Drain()
				expect(test.Text(instance)).to.equal("initial")
				expect((next(instance.Changes))).to.equal(nil)
			end
		end
	end)

	it("preserves edits made between callback version retries", function()
		for _, callbackInput in { "initial", "newer" } do
			local test = fixture()
			local instance = test.Script("initial")
			test.Set(instance, "older")
			test.Start()
			test.Step(instance, "attempt")
			instance:Edit("newer")
			test.Step(instance, "commit")
			-- Both stale and refreshed callback inputs must leave the draft untouched.
			test.Step(instance, "attempt", callbackInput)
			test.Drain()
			expect(test.Text(instance)).to.equal("newer")
		end
	end)

	it("applies an authoritative patch over an existing draft despite stale callback input", function()
		local test = fixture()
		local instance = test.Script("initial")
		instance:Edit("existing draft")
		test.Set(instance, "filesystem conflict resolution")
		test.Start()
		test.Step(instance, "attempt", "initial")
		test.Step(instance, "attempt", "existing draft")
		test.Drain()
		expect(test.Text(instance)).to.equal("filesystem conflict resolution")
	end)

	it("converges to the latest of several patches queued before any callback", function()
		for count = 2, 3 do
			local test = fixture()
			local instance = test.Script("initial")
			for index = 1, count do
				test.Set(instance, tostring(index))
			end
			test.Drain()
			expect(test.Text(instance)).to.equal(tostring(count))
		end
	end)

	it("carries a completed preceding remote commit into the latest pending patch", function()
		for count = 2, 3 do
			local test = fixture()
			local instance = test.Script("initial")
			test.Set(instance, "first")
			test.Start()
			test.Step(instance, "attempt")
			for index = 2, count do
				test.Set(instance, tostring(index))
			end
			test.Step(instance, "commit")
			test.Drain()
			expect(test.Text(instance)).to.equal(tostring(count))
		end
	end)

	it("converges across a chain of separately committed overlapping patches", function()
		for _, edit in { false, true } do
			local test = fixture(true)
			local instance = test.Script("initial")
			test.Set(instance, "first")
			test.Step(instance, "attempt")
			test.Set(instance, "second")
			test.Step(instance, "commit")
			test.Step(instance, "attempt")
			test.Set(instance, "third")
			if edit then
				instance:Edit("newer draft")
			end
			test.Step(instance, "commit")
			test.Drain()
			expect(test.Text(instance)).to.equal(if edit then "newer draft" else "third")
		end
	end)

	it("does not authorize historic remote strings after their requests have completed", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "first")
		test.Drain()
		test.Set(instance, "second")
		test.Drain()
		test.Set(instance, "third")
		instance:Edit("first")
		test.Drain()
		expect(test.Text(instance)).to.equal("first")
	end)

	it("cancels superseded callbacks on retry instead of applying obsolete proposals", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "obsolete")
		test.Start()
		test.Step(instance, "attempt")
		test.Set(instance, "latest")
		test.Step(instance, "attempt")
		test.Drain()
		expect(test.Text(instance)).to.equal("latest")
	end)

	it("does not mistake an uncommitted remote proposal for safe editor text", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "proposed")
		test.Start()
		test.Step(instance, "attempt")
		test.Set(instance, "latest")
		instance:Edit("proposed")
		test.Step(instance, "commit")
		test.Drain()
		expect(test.Text(instance)).to.equal("proposed")
	end)

	it("preserves a draft after several patches even if an earlier remote write completes", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "first")
		test.Start()
		test.Step(instance, "attempt")
		test.Set(instance, "second")
		test.Set(instance, "third")
		test.Step(instance, "commit")
		instance:Edit("newer draft")
		test.Drain()
		expect(test.Text(instance)).to.equal("newer draft")
	end)

	it("allows a newer authoritative patch scheduled after an intervening local edit", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "obsolete")
		test.Start()
		test.Step(instance, "attempt")
		instance:Edit("intervening draft")
		test.Set(instance, "authoritative")
		test.Step(instance, "commit")
		test.Drain()
		expect(test.Text(instance)).to.equal("authoritative")
	end)

	it("preserves another local edit after that newer authoritative patch is scheduled", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "obsolete")
		test.Start()
		test.Step(instance, "attempt")
		instance:Edit("intervening draft")
		test.Set(instance, "authoritative")
		instance:Edit("newest draft")
		test.Step(instance, "commit")
		test.Drain()
		expect(test.Text(instance)).to.equal("newest draft")
	end)

	it("handles patches between a preceding commit and its async return", function()
		local test = fixture()
		local instance = test.Script("initial")
		instance.DelayReturn = true
		test.Set(instance, "first")
		test.Start()
		test.Step(instance, "attempt")
		test.Step(instance, "commit")
		test.Set(instance, "latest")
		test.Drain()
		expect(test.Text(instance)).to.equal("latest")
	end)

	it("applies a patch scheduled over a reverted draft before a preceding async return", function()
		local test = fixture()
		local instance = test.Script("initial")
		instance.DelayReturn = true
		test.Set(instance, "first")
		test.Start()
		test.Step(instance, "attempt")
		test.Step(instance, "commit")
		instance:Edit("initial")
		test.Set(instance, "authoritative")
		test.Drain()
		expect(test.Text(instance)).to.equal("authoritative")
	end)

	it("preserves a draft reverted after the preceding write has returned", function()
		local test = fixture()
		local instance = test.Script("initial")
		instance.DelayReturn = true
		test.Set(instance, "first")
		test.Start()
		test.Step(instance, "attempt")
		test.Set(instance, "latest")
		test.Step(instance, "commit")
		test.Step(instance, "finish")
		instance:Edit("initial")
		test.Drain()
		expect(test.Text(instance)).to.equal("initial")
	end)

	it("preserves a reverted local draft matching a completed preceding remote write", function()
		local test = fixture()
		local instance = test.Script("initial")
		instance.DelayReturn = true
		test.Set(instance, "first")
		test.Start()
		test.Step(instance, "attempt")
		test.Set(instance, "latest")
		test.Step(instance, "commit")
		instance:Edit("newer")
		instance:Edit("first")
		test.Drain()
		expect(test.Text(instance)).to.equal("first")
	end)

	it("preserves edits between a preceding commit and its async return", function()
		local test = fixture()
		local instance = test.Script("initial")
		instance.DelayReturn = true
		test.Set(instance, "first")
		test.Start()
		test.Step(instance, "attempt")
		test.Set(instance, "latest")
		test.Step(instance, "commit")
		instance:Edit("newer draft")
		test.Drain()
		expect(test.Text(instance)).to.equal("newer draft")
	end)

	it("lets an unchanged newest patch supersede an in-flight write", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "obsolete")
		test.Start()
		test.Step(instance, "attempt")
		test.Set(instance, "initial")
		test.Step(instance, "commit")
		test.Drain()
		expect(test.Text(instance)).to.equal("initial")
	end)

	it("leaves unchanged idle scripts alone and accepts later patches", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "initial")
		instance:Edit("draft")
		test.Drain()
		expect(test.Text(instance)).to.equal("draft")
		test.Set(instance, "authoritative")
		test.Drain()
		expect(test.Text(instance)).to.equal("authoritative")
	end)

	it("updates closed documents and protects newer closed-source edits", function()
		for _, edit in { false, true } do
			local test = fixture()
			local instance = test.Script("initial", false)
			test.Set(instance, "remote")
			if edit then
				instance:Edit("newer")
			end
			test.Drain()
			expect(test.Text(instance)).to.equal(if edit then "newer" else "remote")
		end
	end)

	it("does not serialize writes to different scripts behind a yielding editor", function()
		local test = fixture()
		local first = test.Script("initial")
		local second = test.Script("initial")
		test.Set(first, "first remote")
		test.Set(second, "second remote")
		test.Start()
		test.Step(second, "attempt")
		test.Step(second, "commit")
		expect(test.Text(first)).to.equal("initial")
		expect(test.Text(second)).to.equal("second remote")
		test.Drain()
		expect(test.Text(first)).to.equal("first remote")
	end)

	it("cancels writes to destroyed scripts before start and during a retry", function()
		for _, started in { false, true } do
			local test = fixture()
			local instance = test.Script("initial")
			test.Set(instance, "remote")
			if started then
				test.Start()
				test.Step(instance, "attempt")
			end
			instance:Destroy()
			if started then
				test.Step(instance, "attempt")
			end
			test.Drain()
			expect(instance.Text).to.equal("initial")
			expect(#test.Errors).to.equal(0)
		end
	end)

	it("surfaces asynchronous setter errors and allows a subsequent write", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "failed")
		test.Start()
		test.Step(instance, "fail", "Editor permission failure.")
		expect(test.Errors[1]:find("Editor permission failure.", 1, true) ~= nil).to.equal(true)
		test.Set(instance, "recovered")
		test.Drain()
		expect(test.Text(instance)).to.equal("recovered")
	end)

	it("does not strand a newer patch when its predecessor errors", function()
		local test = fixture()
		local instance = test.Script("initial")
		test.Set(instance, "failed")
		test.Start()
		test.Step(instance, "attempt")
		test.Set(instance, "latest")
		test.Step(instance, "fail", "Editor failure.")
		test.Drain()
		expect(test.Errors[1]:find("Editor failure.", 1, true) ~= nil).to.equal(true)
		expect(test.Text(instance)).to.equal("latest")
	end)

	it("surfaces synchronous reads and callback errors without poisoning later writes", function()
		local test = fixture()
		local instance = test.Script("initial")
		instance.ReadError = "Read permission failure."
		local success, err = pcall(test.Set, instance, "failed")
		expect(success).to.equal(false)
		expect(err).to.equal("Read permission failure.")
		instance.ReadError = nil
		test.Set(instance, "failed callback")
		test.Start()
		instance.ReadError = "Callback read failure."
		test.Step(instance, "attempt")
		expect(test.Errors[1]:find("Callback read failure.", 1, true) ~= nil).to.equal(true)
		instance.ReadError = nil
		test.Set(instance, "recovered")
		test.Drain()
		expect(test.Text(instance)).to.equal("recovered")
	end)
end
