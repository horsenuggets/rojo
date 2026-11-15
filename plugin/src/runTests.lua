return function(TestEZ)
	local Rojo = script.Parent.Parent
	local Packages = Rojo.Packages

	TestEZ.TestBootstrap:run({ Rojo.Plugin, Rojo.Stories, Packages.Http, Packages.Log, Packages.RbxDom })
end
