return function(TestEZ)
	local Rojo = script.Parent.Parent
	local Packages = Rojo.Packages

	local roots = { Rojo.Plugin, Packages.Http, Packages.Log, Packages.RbxDom }

	-- Stories only exist in the test place. Dev builds of the plugin also run these tests
	-- and don't include them.
	local stories = Rojo:FindFirstChild("Stories")
	if stories then
		table.insert(roots, stories)
	end

	TestEZ.TestBootstrap:run(roots)
end
