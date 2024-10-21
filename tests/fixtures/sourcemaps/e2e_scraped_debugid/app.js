var makeAFailure=function(){function n(n){}function e(n){throw new Error("failed!")}function r(r){var i=null;if(r.failed){i=e}else{i=n}i(r)}function i(){var n={failed:true,value:42};r(n)}return i}();
//# debugId=2f259f80-58b7-44cb-d7cd-de1505e7e718
