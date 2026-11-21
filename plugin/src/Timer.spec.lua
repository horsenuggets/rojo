return function()
	local Settings = require(script.Parent.Settings)
	local Timer = require(script.Parent.Timer)
	local originalEnabled
	local originalProfiler
	local events
	local active

	local function expectEvents(expected)
		expect(table.concat(events, "| ")).to.equal(table.concat(expected, "| "))
	end

	beforeEach(function()
		originalEnabled = Settings:get("timingLogsEnabled")
		originalProfiler = Timer._profiler
		-- Exercise configuration changes without persisting test values to the plugin store.
		Settings._values.timingLogsEnabled = false
		events = {}
		active = {}
		Timer._profiler = {
			Begin = function(label)
				local thread = coroutine.running()
				active[thread] = (active[thread] or 0) + 1
				table.insert(events, "begin " .. label)
			end,
			End = function()
				local thread = coroutine.running()
				assert((active[thread] or 0) > 0, "No active test profile annotation")
				active[thread] -= 1
				table.insert(events, "end")
			end,
		}
	end)

	afterEach(function()
		while Timer._entries[coroutine.running()] do
			Timer.stop()
		end
		Timer._profiler = originalProfiler
		Settings._values.timingLogsEnabled = originalEnabled
		for _, count in active do
			expect(count).to.equal(0)
		end
	end)

	it("should profile nested CPU scopes even when console timing logs are disabled", function()
		local thread = coroutine.running()
		Timer.start("TimerTest.Outer")
		Timer.start("TimerTest.Inner", "cpu")
		local entries = Timer._entries[thread]
		expect(#entries).to.equal(2)
		expect(entries[1].Profiled).to.equal(true)
		expect(entries[2].Profiled).to.equal(true)
		expect(entries[1].Start).to.equal(nil)
		expect(entries[2].Start).to.equal(nil)
		Timer.stop()
		Timer.stop()
		expect(Timer._entries[thread]).to.equal(nil)
		expectEvents({ "begin Rojo.TimerTest.Outer", "begin Rojo.TimerTest.Inner", "end", "end" })
	end)

	it("should close measured spans on success or error while preserving return values", function()
		for _, mode in { "cpu", "wall" } do
			local values = table.pack(Timer.measure("TimerTest.Measured", mode, function(first, last)
				return first, nil, last
			end, "first", "last"))
			expect(values.n).to.equal(3)
			expect(values[1]).to.equal("first")
			expect(values[2]).to.equal(nil)
			expect(values[3]).to.equal("last")
			local sentinel = {}
			local ok, problem = pcall(Timer.measure, "TimerTest.Error", mode, function()
				error(sentinel)
			end)
			expect(ok).to.equal(false)
			expect(problem).to.equal(sentinel)
			expect(Timer._entries[coroutine.running()]).to.equal(nil)
		end
		expectEvents({ "begin Rojo.TimerTest.Measured", "end", "begin Rojo.TimerTest.Error", "end" })
	end)

	it("should clean measured wall spans after a scheduler yield and cancellation", function()
		local ok = pcall(Timer.measure, "TimerTest.YieldingError", "wall", function()
			task.wait()
			error("The operation was cancelled")
		end)
		expect(ok).to.equal(false)
		expect(Timer._entries[coroutine.running()]).to.equal(nil)
		expectEvents({})
	end)

	it("should keep nested wall spans out of the profiler", function()
		for _, enabled in { false, true } do
			Settings._values.timingLogsEnabled = enabled
			Timer.start("TimerTest.Wall", "wall")
			Timer.start("TimerTest.NestedWall", "wall")
			local entries = Timer._entries[coroutine.running()]
			expect(entries[1].Profiled).to.equal(false)
			expect(entries[2].Profiled).to.equal(false)
			expect(entries[1].Start ~= nil).to.equal(enabled)
			Timer.stop()
			Timer.stop()
		end
		expectEvents({})
	end)

	it("should preserve CPU boundaries through mixed logical nesting", function()
		Timer.start("TimerTest.Wall", "wall")
		Timer.start("TimerTest.Cpu")
		Timer.start("TimerTest.InnerWall", "wall")
		Timer.start("TimerTest.InnerCpu")
		Timer.stop()
		Timer.stop()
		expectEvents({ "begin Rojo.TimerTest.Cpu", "begin Rojo.TimerTest.InnerCpu", "end" })
		expect(active[coroutine.running()]).to.equal(1)
		Timer.stop()
		Timer.stop()
		expectEvents({ "begin Rojo.TimerTest.Cpu", "begin Rojo.TimerTest.InnerCpu", "end", "end" })
	end)

	it("should reject missing or non-string labels without opening a scope", function()
		for _, invalid in { false, 1, {} } do
			expect(function()
				Timer.start(invalid)
			end).to.throw()
			expect(Timer._entries[coroutine.running()]).to.equal(nil)
		end
		expect(function()
			Timer.start()
		end).to.throw()
		expect(Timer._entries[coroutine.running()]).to.equal(nil)
		expectEvents({})
	end)

	it("should reject invalid modes before opening a scope", function()
		for _, mode in { false, 1, {}, "unknown" } do
			expect(function()
				Timer.start("TimerTest.InvalidMode", mode)
			end).to.throw()
		end
		expect(Timer._entries[coroutine.running()]).to.equal(nil)
		expectEvents({})
	end)

	it("should reject an unmatched stop without consuming another coroutine's wall span", function()
		local other = coroutine.create(function()
			Timer.start("TimerTest.Other", "wall")
			coroutine.yield()
			Timer.stop()
		end)
		local success, err = coroutine.resume(other)
		assert(success, err)
		expect(function()
			Timer.stop()
		end).to.throw()
		expect(Timer._entries[other][1].Label).to.equal("TimerTest.Other")
		success, err = coroutine.resume(other)
		assert(success, err)
		expect(Timer._entries[other]).to.equal(nil)
		expectEvents({})
	end)

	it("should keep interleaved wall spans and synchronous CPU scopes separate", function()
		for _, enabled in { false, true } do
			Settings._values.timingLogsEnabled = enabled
			local function worker(label)
				return coroutine.create(function()
					Timer._start(label, "wall")
					Timer.start(label .. ".Before")
					Timer.stop()
					coroutine.yield()
					Timer.start(label .. ".After")
					Timer.stop()
					Timer._stop()
				end)
			end
			local first = worker("TimerTest.First")
			local second = worker("TimerTest.Second")
			local success, err = coroutine.resume(first)
			assert(success, err)
			success, err = coroutine.resume(second)
			assert(success, err)
			expect(Timer._entries[first][1].Label).to.equal("TimerTest.First")
			expect(Timer._entries[second][1].Label).to.equal("TimerTest.Second")
			expect(Timer._entries[first] == Timer._entries[second]).to.equal(false)
			expect(active[first]).to.equal(0)
			expect(active[second]).to.equal(0)
			success, err = coroutine.resume(first)
			assert(success, err)
			expect(Timer._entries[first]).to.equal(nil)
			expect(Timer._entries[second][1].Label).to.equal("TimerTest.Second")
			success, err = coroutine.resume(second)
			assert(success, err)
			expect(Timer._entries[second]).to.equal(nil)
		end
		local expected = {}
		for _ = 1, 2 do
			for _, label in { "First.Before", "Second.Before", "First.After", "Second.After" } do
				table.insert(expected, "begin Rojo.TimerTest." .. label)
				table.insert(expected, "end")
			end
		end
		expectEvents(expected)
	end)

	it("should leave no CPU annotation open across a real scheduler yield", function()
		Timer.start("TimerTest.Scheduled", "wall")
		Timer.start("TimerTest.BeforeWait")
		Timer.stop()
		local scheduled = false
		task.defer(function()
			scheduled = true
		end)
		task.wait()
		expect(scheduled).to.equal(true)
		expect(active[coroutine.running()]).to.equal(0)
		expect(Timer._entries[coroutine.running()][1].Profiled).to.equal(false)
		Timer.start("TimerTest.AfterWait")
		Timer.stop()
		Timer.stop()
		expectEvents({ "begin Rojo.TimerTest.BeforeWait", "end", "begin Rojo.TimerTest.AfterWait", "end" })
	end)

	it("should retain logging and profiling choices across configuration changes", function()
		for _, enabled in { false, true } do
			for _, mode in { "cpu", "wall" } do
				Settings._values.timingLogsEnabled = enabled
				Timer.start("TimerTest.BeforeSettingChange", mode)
				Settings._values.timingLogsEnabled = not enabled
				Timer.start("TimerTest.AfterSettingChange", mode)
				local entries = Timer._entries[coroutine.running()]
				expect(entries[1].Start ~= nil).to.equal(enabled)
				expect(entries[2].Start ~= nil).to.equal(not enabled)
				expect(entries[1].Profiled).to.equal(mode == "cpu")
				Timer.stop()
				Timer.stop()
				expect(Timer._entries[coroutine.running()]).to.equal(nil)
			end
		end
		local expected = {}
		for _ = 1, 2 do
			table.insert(expected, "begin Rojo.TimerTest.BeforeSettingChange")
			table.insert(expected, "begin Rojo.TimerTest.AfterSettingChange")
			table.insert(expected, "end")
			table.insert(expected, "end")
		end
		expectEvents(expected)
	end)

	-- A tiny DOM keeps these tests about the reconciler's callback/timer boundary.
	local function walk(method, step)
		local Reconciler = require(script.Parent.Reconciler)
		local root = {
			ClassName = "Folder",
			Name = "Root",
			GetChildren = function()
				return {}
			end,
		}
		local map = { fromIds = {}, fromInstances = {} }
		function map:insert(id, instance)
			self.fromIds[id] = instance
			self.fromInstances[instance] = id
		end
		map:insert("ROOT", root)
		local reconciler = Reconciler.new(map)
		local virtual = { ROOT = { ClassName = "Folder", Name = "Root", Properties = {}, Children = {} } }
		if method == "hydrate" then
			return reconciler:hydrate(virtual, "ROOT", root, step)
		else
			return reconciler:diff(virtual, "ROOT", step)
		end
	end

	it("should keep yielding hydration and diff callbacks outside CPU scopes", function()
		local yieldBudget = require(script.Parent.yieldBudget)
		for _, method in { "hydrate", "diff" } do
			local yielded = false
			local budget = yieldBudget(0)
			walk(method, function()
				expect(#events).to.equal(0)
				expect(Timer._entries[coroutine.running()][1].Profiled).to.equal(false)
				yielded = budget()
			end)
			expect(yielded).to.equal(true)
			expect(Timer._entries[coroutine.running()]).to.equal(nil)
		end
		expectEvents({})
	end)

	it("should clean up cancelled walks after a scheduler yield and preserve the error", function()
		local cancelled = {}
		for _, method in { "hydrate", "diff" } do
			Timer.start("TimerTest.Parent", "wall")
			local ok, err = pcall(walk, method, function()
				task.wait()
				error(cancelled, 0)
			end)
			expect(ok).to.equal(false)
			expect(err).to.equal(cancelled)
			local entries = Timer._entries[coroutine.running()]
			expect(#entries).to.equal(1)
			expect(entries[1].Label).to.equal("TimerTest.Parent")
			Timer.stop()
		end
		expectEvents({})
	end)

	it("should retain CPU scopes for synchronous walks without callbacks", function()
		walk("hydrate")
		walk("diff")
		expectEvents({ "begin Rojo.Reconciler:hydrate", "end", "begin Rojo.Reconciler:diff", "end" })
	end)

	it("should close a synchronous patch scope on error without closing its parent", function()
		local Reconciler = require(script.Parent.Reconciler)
		Timer.start("TimerTest.Parent", "wall")
		local ok = pcall(Reconciler.applyPatch, Reconciler.new({}), nil)
		expect(ok).to.equal(false)
		expect(#Timer._entries[coroutine.running()]).to.equal(1)
		expectEvents({ "begin Rojo.Reconciler:applyPatch", "end" })
		Timer.stop()
		expectEvents({ "begin Rojo.Reconciler:applyPatch", "end" })
	end)

	it("should release completed stacks and weakly reference suspended coroutines", function()
		expect(getmetatable(Timer._entries).__mode).to.equal("k")
		Timer.start("TimerTest.Completed")
		Timer.stop()
		expect(Timer._entries[coroutine.running()]).to.equal(nil)
		expect(function()
			Timer.stop()
		end).to.throw()
		expectEvents({ "begin Rojo.TimerTest.Completed", "end" })
	end)
end
