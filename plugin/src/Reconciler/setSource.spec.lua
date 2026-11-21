--[[
	Tests Source writes with a deterministic editor and scheduler. Proposals, version
	rejections, commits, and async returns are separate steps, without Studio services.
]]

return function()
	local createSourceSetter = require(script.Parent.setSource)

	local function fixture(eager)
		local ready = {}
		local errors = {}
		local scripts = {}
		local resume
		local editor = {}
		local test = { Errors = errors }

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
						for _, changed in instance.Changes do
							changed()
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

		test.Set = createSourceSetter(editor)
		test.Text = editor.GetEditorSource

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

		function test.Script(initial, open)
			local listeners = {}
			local instance = {
				Changes = {},
				Destroying = {},
				Open = open ~= false,
				Source = initial,
				Text = initial,
				Version = 0,
			}
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
				for _, changed in self.Changes do
					changed()
				end
			end
			table.insert(scripts, instance)
			return instance
		end

		return test
	end

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
				expect(next(instance.Changes)).to.equal(nil)
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
